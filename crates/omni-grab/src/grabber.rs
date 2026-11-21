//! El capturador: analiza lo que se pega o se copia, agrupa los archivos
//! encontrados en paquetes y deja elegir cuáles descargar y dónde.
//!
//! Como en JDownloader, los enlaces de una página forman un paquete con el
//! título de la página, y los enlaces sueltos se agrupan por nombre
//! (`fotos.part1.rar` y `fotos.part2.rar` van juntos).
//!
//! Vive en el servicio `omnid`, junto a la cola: así lo que se añade desde
//! la ventana, la terminal o (más adelante) el navegador va al mismo sitio,
//! y la lista se guarda aunque se cierre la ventana. No sabe nada de la
//! interfaz.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use omni_queue::{NewJob, Priority};
use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;
use tokio::sync::Semaphore;
use url::Url;

use crate::check::Checkers;
use crate::inspect::Found;
use crate::links::extract_urls;

/// Comprobaciones a la vez en total, sumando todos los servidores.
pub const PARALLEL_CHECKS: usize = 24;

/// En qué punto está un enlace encontrado.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "detail", rename_all = "lowercase")]
pub enum Check {
    Checking,
    /// Disponible; `resumable` si acepta rangos (varias conexiones y
    /// reanudación).
    Online {
        resumable: bool,
    },
    Offline(String),
}

/// Dónde se guardan los paquetes: la carpeta de descargas y, si se quiere,
/// una subcarpeta con el nombre de cada paquete.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Dest {
    pub base: PathBuf,
    /// Una subcarpeta por paquete.
    pub per_package: bool,
    /// También cuando el paquete tiene un solo archivo.
    pub single_file_too: bool,
}

impl Dest {
    /// La carpeta por defecto de un paquete con `files` archivos. El nombre
    /// se limpia de caracteres que Windows, Linux o macOS no admiten.
    pub fn folder(&self, package: &str, files: usize) -> PathBuf {
        if self.per_package && (files > 1 || self.single_file_too) {
            self.base.join(omni_core::filename::sanitize(package))
        } else {
            self.base.clone()
        }
    }
}

/// Un archivo encontrado, para pintarlo.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GrabItem {
    pub id: u64,
    pub name: String,
    /// El servidor o, si lo comprueba un plugin, el nombre del host
    /// ("Mega").
    pub host: String,
    pub url: String,
    pub size: Option<u64>,
    pub check: Check,
    pub selected: bool,
    /// Ya está en la lista de descargas.
    pub duplicate: bool,
    /// Tanda de enlaces copiados (0: ninguna).
    pub batch: u64,
}

/// Un paquete del capturador, para pintarlo.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GrabPackage {
    pub id: u64,
    pub name: String,
    /// Carpeta elegida a mano, si se eligió.
    pub custom_folder: Option<PathBuf>,
    pub priority: Priority,
    pub items: Vec<GrabItem>,
}

impl GrabPackage {
    /// La carpeta del paquete: la elegida a mano o la que toca según `dest`.
    pub fn folder(&self, dest: &Dest) -> PathBuf {
        self.custom_folder
            .clone()
            .unwrap_or_else(|| dest.folder(&self.name, self.items.len()))
    }
}

/// Un archivo elegido para descargar.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Picked {
    pub url: String,
    pub name: String,
    pub package: String,
    pub folder: PathBuf,
    pub priority: Priority,
    /// Tamaño visto al comprobar el enlace.
    pub size: Option<u64>,
}

impl Picked {
    /// La descarga que le corresponde en la cola.
    pub fn into_job(self, start: bool) -> NewJob {
        NewJob {
            name: Some(self.name),
            package: self.package,
            priority: self.priority,
            size: self.size,
            start,
            ..NewJob::new(self.url, self.folder)
        }
    }
}

/// Qué archivos sacar del capturador.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pick {
    /// Todo lo marcado.
    Selected,
    /// Lo marcado y disponible de una tanda de enlaces copiados.
    Batch(u64),
}

/// Una orden al capturador. La API del servicio las recibe tal cual.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum GrabCommand {
    SelectItem {
        id: u64,
        selected: bool,
    },
    SelectPackage {
        id: u64,
        selected: bool,
    },
    SelectAll {
        selected: bool,
    },
    RemovePackage {
        id: u64,
    },
    RemoveOffline,
    DiscardBatch {
        batch: u64,
    },
    Clear,
    RenamePackage {
        id: u64,
        name: String,
    },
    /// Carpeta del paquete; vacía = la de los ajustes.
    SetPackageFolder {
        id: u64,
        folder: PathBuf,
    },
    SetPackagePriority {
        id: u64,
        priority: Priority,
    },
    /// Vuelve a comprobar estos enlaces (vacío = los no disponibles).
    Recheck {
        ids: Vec<u64>,
    },
}

/// Lo que devuelve [`Grabber::add_links`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Added {
    /// Enlaces nuevos que se van a comprobar.
    pub added: usize,
    /// Tanda a la que van (0: ninguna).
    pub batch: u64,
}

/// Dice si un enlace ya está en la lista de descargas.
pub type KnownLinks = Arc<dyn Fn(&str) -> bool + Send + Sync>;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Package {
    id: u64,
    name: String,
    /// `true` si se creó agrupando enlaces sueltos por nombre (y no se ha
    /// renombrado): los archivos pueden cambiar de paquete al saber su nombre
    /// real, y se borra al quedarse vacío.
    auto: bool,
    folder: Option<PathBuf>,
    priority: Priority,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Item {
    id: u64,
    package: u64,
    url: Url,
    name: String,
    host: String,
    size: Option<u64>,
    check: Check,
    selected: bool,
    #[serde(default)]
    duplicate: bool,
    /// Tanda de enlaces copiados a la que pertenece (0: ninguna), para el
    /// aviso de abajo a la derecha.
    batch: u64,
    /// Si es una página, ¿hay que mirar sus enlaces? (No se siguen páginas
    /// dentro de páginas.)
    #[serde(default)]
    follow: bool,
}

impl Item {
    fn view(&self) -> GrabItem {
        GrabItem {
            id: self.id,
            name: self.name.clone(),
            host: self.host.clone(),
            url: self.url.to_string(),
            size: self.size,
            check: self.check.clone(),
            selected: self.selected,
            duplicate: self.duplicate,
            batch: self.batch,
        }
    }

    fn selectable(&self) -> bool {
        !matches!(self.check, Check::Offline(_))
    }
}

/// Lo que se guarda del capturador para la próxima vez.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Saved {
    packages: Vec<Package>,
    items: Vec<Item>,
}

#[derive(Default)]
struct State {
    packages: Vec<Package>,
    items: Vec<Item>,
}

impl State {
    fn package_named(&mut self, ids: &AtomicU64, name: &str, auto: bool) -> u64 {
        if let Some(p) = self
            .packages
            .iter()
            .find(|p| p.name == name && p.auto == auto)
        {
            return p.id;
        }
        let id = ids.fetch_add(1, Ordering::Relaxed);
        self.packages.push(Package {
            id,
            name: name.to_owned(),
            auto,
            folder: None,
            priority: Priority::Normal,
        });
        id
    }

    fn drop_empty_packages(&mut self) {
        let items = &self.items;
        self.packages
            .retain(|p| items.iter().any(|i| i.package == p.id));
    }

    fn package_mut(&mut self, id: u64) -> Option<&mut Package> {
        self.packages.iter_mut().find(|p| p.id == id)
    }
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    /// Avisos para el usuario ("esa página no tiene archivos").
    notes: Mutex<Vec<String>>,
    /// Sube con cada cambio, para saber cuándo guardar.
    generation: AtomicU64,
    /// Un semáforo por servidor (o por plugin).
    per_host: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl Shared {
    /// El estado, para cambiarlo.
    fn state(&self) -> MutexGuard<'_, State> {
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.view()
    }

    /// El estado, solo para leerlo.
    fn view(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("capturador envenenado")
    }

    fn note(&self, msg: String) {
        self.notes.lock().expect("avisos envenenados").push(msg);
    }

    fn host_limit(&self, key: &str, parallel: usize) -> Arc<Semaphore> {
        self.per_host
            .lock()
            .expect("semáforos envenenados")
            .entry(key.to_owned())
            .or_insert_with(|| Arc::new(Semaphore::new(parallel)))
            .clone()
    }
}

/// El capturador. Se clona barato: los clones comparten la misma lista.
#[derive(Clone)]
pub struct Grabber {
    rt: Handle,
    client: reqwest::Client,
    checkers: Checkers,
    known: Option<KnownLinks>,
    shared: Arc<Shared>,
    ids: Arc<AtomicU64>,
    batches: Arc<AtomicU64>,
    limit: Arc<Semaphore>,
}

impl Grabber {
    /// `client` es el de la red por la que se comprueban los enlaces.
    pub fn new(rt: Handle, client: reqwest::Client, checkers: Checkers) -> Self {
        Self {
            rt,
            client,
            checkers,
            known: None,
            shared: Arc::default(),
            ids: Arc::new(AtomicU64::new(1)),
            batches: Arc::new(AtomicU64::new(1)),
            limit: Arc::new(Semaphore::new(PARALLEL_CHECKS)),
        }
    }

    /// Para avisar de los enlaces que ya están en la lista de descargas:
    /// salen desmarcados.
    pub fn with_known_links(mut self, known: KnownLinks) -> Self {
        self.known = Some(known);
        self
    }

    fn is_known(&self, url: &Url) -> bool {
        self.known.as_ref().is_some_and(|k| k(url.as_str()))
    }

    /// Analiza los enlaces de `text`. Con `new_batch`, forman una tanda
    /// nueva (la del aviso de enlaces copiados).
    pub fn add_links(&self, text: &str, new_batch: bool) -> Result<Added, String> {
        let batch = if new_batch {
            self.batches.fetch_add(1, Ordering::Relaxed)
        } else {
            0
        };
        self.add_to_batch(text, batch)
            .map(|added| Added { added, batch })
    }

    /// Como [`Self::add_links`], en una tanda que ya existe (0: ninguna).
    /// Devuelve cuántos enlaces nuevos se van a comprobar.
    pub fn add_to_batch(&self, text: &str, batch: u64) -> Result<usize, String> {
        let urls = extract_urls(text);
        if urls.is_empty() {
            return Err("No encuentro ningún enlace http o https en lo que has pegado.".into());
        }
        let mut added = 0;
        for url in urls {
            let name = guess_name(&url);
            let route = self.checkers.route(&url);
            let duplicate = self.is_known(&url);
            let id = {
                let mut st = self.shared.state();
                if st.items.iter().any(|i| i.url == url) {
                    continue;
                }
                let package = st.package_named(&self.ids, &package_name(&name), true);
                let id = self.ids.fetch_add(1, Ordering::Relaxed);
                st.items.push(Item {
                    id,
                    package,
                    url: url.clone(),
                    name,
                    host: route.label,
                    size: None,
                    check: Check::Checking,
                    selected: !duplicate,
                    duplicate,
                    batch,
                    follow: true,
                });
                id
            };
            added += 1;
            self.spawn_check(id, url, true);
        }
        if added == 0 {
            return Err("Esos enlaces ya están en el capturador.".into());
        }
        Ok(added)
    }

    /// Comprueba un enlace en segundo plano. Si es una página y
    /// `follow_pages`, sus enlaces a archivos pasan a la lista en su lugar,
    /// en un paquete con el título de la página.
    fn spawn_check(&self, id: u64, url: Url, follow_pages: bool) {
        let me = self.clone();
        self.rt.spawn(async move {
            let route = me.checkers.route(&url);
            let found = {
                let host = me.shared.host_limit(&route.key, route.parallel);
                let _host = host.acquire_owned().await;
                let _all = me.limit.acquire().await;
                me.checkers.check(&me.client, &url).await
            };
            match found {
                Ok(Found::File(info)) => me.found_file(id, info),
                Ok(Found::Page {
                    url: page,
                    title,
                    links,
                }) => me.found_page(id, follow_pages, page, title, links),
                Ok(Found::Folder { name, files }) => me.found_folder(id, &url, name, files),
                Err(e) => {
                    let mut st = me.shared.state();
                    if let Some(item) = st.items.iter_mut().find(|i| i.id == id) {
                        item.check = Check::Offline(short_error(&e));
                        item.selected = false;
                    }
                }
            }
        });
    }

    fn found_file(&self, id: u64, info: omni_core::RemoteInfo) {
        let mut st = self.shared.state();
        let Some(i) = st.items.iter().position(|i| i.id == id) else {
            return; // lo quitaron mientras tanto
        };
        // Un enlace suelto se agrupa por su nombre real.
        let auto = st
            .packages
            .iter()
            .any(|p| p.id == st.items[i].package && p.auto);
        if auto {
            let pkg = st.package_named(&self.ids, &package_name(&info.filename), true);
            st.items[i].package = pkg;
        }
        let item = &mut st.items[i];
        item.name = info.filename.clone();
        item.size = info.size;
        item.check = Check::Online {
            resumable: info.is_segmentable(),
        };
        st.drop_empty_packages();
    }

    fn found_page(
        &self,
        id: u64,
        follow_pages: bool,
        page: Url,
        title: Option<String>,
        links: Vec<Url>,
    ) {
        let mut st = self.shared.state();
        let Some(batch) = st.items.iter().find(|i| i.id == id).map(|i| i.batch) else {
            return;
        };
        st.items.retain(|i| i.id != id);
        st.drop_empty_packages();
        if !follow_pages {
            // Un enlace de una página que resulta ser otra página: no lo
            // seguimos.
            return;
        }
        if links.is_empty() {
            drop(st);
            self.shared.note(format!(
                "En {} no hay enlaces a archivos.",
                page.host_str().unwrap_or("esa página")
            ));
            return;
        }
        let name = title.unwrap_or_else(|| page.host_str().unwrap_or("Página").to_owned());
        let package = st.package_named(&self.ids, &name, false);
        let mut new = Vec::new();
        for link in links {
            if st.items.iter().any(|i| i.url == link) {
                continue;
            }
            let id = self.ids.fetch_add(1, Ordering::Relaxed);
            let duplicate = self.is_known(&link);
            st.items.push(Item {
                id,
                package,
                url: link.clone(),
                name: guess_name(&link),
                host: self.checkers.route(&link).label,
                size: None,
                check: Check::Checking,
                selected: !duplicate,
                duplicate,
                batch,
                follow: false,
            });
            new.push((id, link));
        }
        drop(st);
        for (id, link) in new {
            self.spawn_check(id, link, false);
        }
    }

    fn found_folder(
        &self,
        id: u64,
        url: &Url,
        name: Option<String>,
        files: Vec<crate::FolderFile>,
    ) {
        let label = self.checkers.route(url).label;
        let mut st = self.shared.state();
        let Some(batch) = st.items.iter().find(|i| i.id == id).map(|i| i.batch) else {
            return;
        };
        st.items.retain(|i| i.id != id);
        st.drop_empty_packages();
        if files.is_empty() {
            drop(st);
            self.shared
                .note(format!("La carpeta de {label} está vacía."));
            return;
        }
        let name = name.unwrap_or_else(|| label.clone());
        let package = st.package_named(&self.ids, &name, false);
        for f in files {
            if st.items.iter().any(|i| i.url == f.url) {
                continue;
            }
            let duplicate = self.is_known(&f.url);
            let id = self.ids.fetch_add(1, Ordering::Relaxed);
            st.items.push(Item {
                id,
                package,
                url: f.url,
                name: f.name,
                host: label.clone(),
                size: f.size,
                check: Check::Online {
                    resumable: f.resumable,
                },
                selected: !duplicate,
                duplicate,
                batch,
                follow: false,
            });
        }
    }

    /// Aplica una orden de la interfaz.
    pub fn apply(&self, cmd: GrabCommand) {
        match cmd {
            GrabCommand::SelectItem { id, selected } => self.set_item_selected(id, selected),
            GrabCommand::SelectPackage { id, selected } => self.set_package_selected(id, selected),
            GrabCommand::SelectAll { selected } => self.select_all(selected),
            GrabCommand::RemovePackage { id } => self.remove_package(id),
            GrabCommand::RemoveOffline => self.remove_offline(),
            GrabCommand::DiscardBatch { batch } => self.discard_batch(batch),
            GrabCommand::Clear => self.clear(),
            GrabCommand::RenamePackage { id, name } => self.rename_package(id, &name),
            GrabCommand::SetPackageFolder { id, folder } => self.set_package_folder(id, folder),
            GrabCommand::SetPackagePriority { id, priority } => {
                self.set_package_priority(id, priority)
            }
            GrabCommand::Recheck { ids } => self.recheck(&ids),
        }
    }

    pub fn set_item_selected(&self, id: u64, selected: bool) {
        if let Some(item) = self.shared.state().items.iter_mut().find(|i| i.id == id)
            && item.selectable()
        {
            item.selected = selected;
        }
    }

    pub fn set_package_selected(&self, package: u64, selected: bool) {
        for item in self
            .shared
            .state()
            .items
            .iter_mut()
            .filter(|i| i.package == package)
        {
            item.selected = selected && item.selectable();
        }
    }

    pub fn select_all(&self, selected: bool) {
        for item in self.shared.state().items.iter_mut() {
            item.selected = selected && item.selectable();
        }
    }

    pub fn remove_package(&self, package: u64) {
        let mut st = self.shared.state();
        st.items.retain(|i| i.package != package);
        st.drop_empty_packages();
    }

    /// Quita los enlaces que no están disponibles.
    pub fn remove_offline(&self) {
        let mut st = self.shared.state();
        st.items.retain(|i| i.selectable());
        st.drop_empty_packages();
    }

    pub fn clear(&self) {
        let mut st = self.shared.state();
        st.items.clear();
        st.packages.clear();
    }

    pub fn rename_package(&self, package: u64, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        if let Some(p) = self.shared.state().package_mut(package) {
            p.name = name.to_owned();
            p.auto = false;
        }
    }

    pub fn set_package_folder(&self, package: u64, folder: PathBuf) {
        if let Some(p) = self.shared.state().package_mut(package) {
            p.folder = (!folder.as_os_str().is_empty()).then_some(folder);
        }
    }

    pub fn set_package_priority(&self, package: u64, priority: Priority) {
        if let Some(p) = self.shared.state().package_mut(package) {
            p.priority = priority;
        }
    }

    /// Vuelve a comprobar enlaces (por ejemplo, los que fallaron porque se
    /// cayó la conexión). Con `ids` vacío, todos los no disponibles.
    pub fn recheck(&self, ids: &[u64]) {
        let todo: Vec<(u64, Url, bool)> = {
            let mut st = self.shared.state();
            st.items
                .iter_mut()
                .filter(|i| {
                    if ids.is_empty() {
                        !i.selectable()
                    } else {
                        ids.contains(&i.id) && i.check != Check::Checking
                    }
                })
                .map(|i| {
                    i.check = Check::Checking;
                    (i.id, i.url.clone(), i.follow)
                })
                .collect()
        };
        for (id, url, follow) in todo {
            self.spawn_check(id, url, follow);
        }
    }

    /// Saca del capturador lo que diga `pick` y lo devuelve para
    /// descargarlo. Cada paquete va a la carpeta que diga `dest`, salvo que
    /// se haya elegido otra a mano.
    pub fn take(&self, pick: Pick, dest: &Dest) -> Vec<Picked> {
        match pick {
            Pick::Selected => self.take_where(dest, |i| i.selected),
            // Lo que aún se comprueba se queda en el capturador.
            Pick::Batch(batch) => self.take_where(dest, |i| {
                i.batch == batch && i.selected && matches!(i.check, Check::Online { .. })
            }),
        }
    }

    /// Quita del capturador todos los enlaces de una tanda.
    pub fn discard_batch(&self, batch: u64) {
        let mut st = self.shared.state();
        st.items.retain(|i| i.batch != batch);
        st.drop_empty_packages();
    }

    /// Los archivos de una tanda, el más reciente primero.
    pub fn batch_items(&self, batch: u64) -> Vec<GrabItem> {
        let st = self.shared.view();
        let mut items: Vec<GrabItem> = st
            .items
            .iter()
            .filter(|i| i.batch == batch)
            .map(Item::view)
            .collect();
        items.sort_by_key(|i| std::cmp::Reverse(i.id));
        items
    }

    fn take_where(&self, dest: &Dest, pick: impl Fn(&Item) -> bool) -> Vec<Picked> {
        let mut st = self.shared.state();
        let mut picked = Vec::new();
        let packages: Vec<(u64, String, PathBuf, Priority)> = st
            .packages
            .iter()
            .map(|p| {
                (
                    p.id,
                    p.name.clone(),
                    folder_of(p, &st.items, dest),
                    p.priority,
                )
            })
            .collect();
        st.items.retain(|i| {
            if !pick(i) {
                return true;
            }
            if let Some((_, name, folder, prio)) = packages.iter().find(|p| p.0 == i.package) {
                picked.push(Picked {
                    url: i.url.to_string(),
                    name: i.name.clone(),
                    package: name.clone(),
                    folder: folder.clone(),
                    priority: *prio,
                    size: i.size,
                });
            }
            false
        });
        st.drop_empty_packages();
        picked
    }

    /// Avisos pendientes; se vacían al leerlos.
    pub fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut *self.shared.notes.lock().expect("avisos envenenados"))
    }

    /// Los paquetes con sus archivos, en el orden en que llegaron.
    pub fn snapshot(&self) -> Vec<GrabPackage> {
        let st = self.shared.view();
        st.packages
            .iter()
            .map(|p| GrabPackage {
                id: p.id,
                name: p.name.clone(),
                custom_folder: p.folder.clone(),
                priority: p.priority,
                items: st
                    .items
                    .iter()
                    .filter(|i| i.package == p.id)
                    .map(Item::view)
                    .collect(),
            })
            .filter(|p| !p.items.is_empty())
            .collect()
    }

    /// Nombres de los plugins de hosts instalados.
    pub fn hosts(&self) -> Vec<String> {
        self.checkers.names()
    }

    /// Sube con cada cambio de la lista.
    pub fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::Relaxed)
    }

    /// La lista, para guardarla.
    pub fn save(&self) -> Saved {
        let st = self.shared.view();
        Saved {
            packages: st.packages.clone(),
            items: st.items.clone(),
        }
    }

    /// Recupera una lista guardada. Lo que se estaba comprobando al guardar
    /// se vuelve a comprobar; las tandas del aviso ya no tienen sentido.
    pub fn restore(&self, saved: Saved) {
        let next = saved
            .packages
            .iter()
            .map(|p| p.id)
            .chain(saved.items.iter().map(|i| i.id))
            .max()
            .unwrap_or(0)
            + 1;
        self.ids.fetch_max(next, Ordering::Relaxed);
        let pending: Vec<(u64, Url, bool)> = {
            let mut st = self.shared.state();
            st.packages = saved.packages;
            st.items = saved.items;
            for i in &mut st.items {
                i.batch = 0;
                i.duplicate = self.is_known(&i.url);
                if i.duplicate {
                    i.selected = false;
                }
            }
            st.drop_empty_packages();
            st.items
                .iter()
                .filter(|i| i.check == Check::Checking)
                .map(|i| (i.id, i.url.clone(), i.follow))
                .collect()
        };
        for (id, url, follow) in pending {
            self.spawn_check(id, url, follow);
        }
    }
}

/// La carpeta elegida a mano para el paquete o, si no, la de los ajustes.
fn folder_of(p: &Package, items: &[Item], dest: &Dest) -> PathBuf {
    p.folder.clone().unwrap_or_else(|| {
        let files = items.iter().filter(|i| i.package == p.id).count();
        dest.folder(&p.name, files)
    })
}

fn guess_name(url: &Url) -> String {
    omni_core::filename::from_url(url)
        .unwrap_or_else(|| url.host_str().unwrap_or("archivo").to_owned())
}

/// Nombre de paquete para un archivo suelto: el nombre sin extensiones ni
/// numeración de partes, para que los volúmenes de un mismo archivo vayan
/// juntos ("fotos.part2.rar" → "fotos", "copia.7z.001" → "copia").
pub fn package_name(file: &str) -> String {
    let mut stem = file;
    // Hasta tres extensiones: ".part1.rar", ".tar.gz", ".7z.001".
    for _ in 0..3 {
        let Some((head, ext)) = stem.rsplit_once('.') else {
            break;
        };
        let lower = ext.to_ascii_lowercase();
        let is_part = lower
            .strip_prefix("part")
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        let is_ext = !head.is_empty()
            && (ext.len() <= 5 || is_part)
            && (lower.chars().all(|c| c.is_ascii_alphanumeric()))
            && (lower.chars().any(|c| c.is_ascii_alphabetic())
                || lower.chars().all(|c| c.is_ascii_digit()) && lower.len() == 3);
        if !is_ext {
            break;
        }
        stem = head;
    }
    if stem.is_empty() {
        file.to_owned()
    } else {
        stem.to_owned()
    }
}

/// El motivo de que un enlace no esté disponible, en pocas palabras.
pub fn short_error(e: &omni_core::Error) -> String {
    use omni_core::Error;
    match e {
        Error::Network(n) if n.is_connect() => "no se pudo conectar".into(),
        Error::Network(n) if n.is_timeout() => "el servidor no responde".into(),
        Error::LinkExpired { status: 404 | 410 } => "no existe (404)".into(),
        Error::LinkExpired { status } => format!("acceso denegado ({status})"),
        Error::HttpStatus { status } => format!("error del servidor ({status})"),
        Error::RetriesExhausted(inner) => short_error(inner),
        other => other.to_string(),
    }
}
