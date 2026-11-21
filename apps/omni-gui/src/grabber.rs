//! Capturador de enlaces de la ventana: analiza lo que se pega, agrupa los
//! archivos encontrados en paquetes y deja elegir cuáles descargar y dónde.
//!
//! Como en JDownloader, los enlaces de una página forman un paquete con el
//! título de la página, y los enlaces sueltos se agrupan por nombre
//! (`fotos.part1.rar` y `fotos.part2.rar` van juntos).
//!
//! Igual que [`crate::manager`], no sabe nada de Slint.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use omni_grab::{Found, extract_urls, inspect};
use tokio::runtime::Handle;
use tokio::sync::Semaphore;
use url::Url;

use crate::manager::Priority;

/// Enlaces que se comprueban a la vez.
const PARALLEL_CHECKS: usize = 8;

/// En qué punto está un enlace encontrado.
#[derive(Clone, Debug, PartialEq, Eq)]
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
#[derive(Clone, Debug, PartialEq)]
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
#[derive(Clone, Debug, PartialEq)]
pub struct GrabItem {
    pub id: u64,
    pub name: String,
    pub host: String,
    pub size: Option<u64>,
    pub check: Check,
    pub selected: bool,
}

/// Un paquete del capturador, para pintarlo.
#[derive(Clone, Debug, PartialEq)]
pub struct GrabPackage {
    pub id: u64,
    pub name: String,
    pub folder: PathBuf,
    pub priority: Priority,
    pub items: Vec<GrabItem>,
}

/// Un archivo elegido para descargar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picked {
    pub url: String,
    pub name: String,
    pub package: String,
    pub folder: PathBuf,
    pub priority: Priority,
    /// Tamaño visto al comprobar el enlace.
    pub size: Option<u64>,
}

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

struct Item {
    id: u64,
    package: u64,
    url: Url,
    name: String,
    size: Option<u64>,
    check: Check,
    selected: bool,
    /// Tanda de enlaces copiados a la que pertenece (0: ninguna), para el
    /// aviso de abajo a la derecha.
    batch: u64,
}

impl Item {
    fn view(&self) -> GrabItem {
        GrabItem {
            id: self.id,
            name: self.name.clone(),
            host: self.url.host_str().unwrap_or("").to_owned(),
            size: self.size,
            check: self.check.clone(),
            selected: self.selected,
        }
    }
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
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    /// Avisos para el usuario ("esa página no tiene archivos").
    notes: Mutex<Vec<String>>,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("capturador envenenado")
    }
}

/// El capturador.
pub struct Grabber {
    rt: Handle,
    client: reqwest::Client,
    shared: Arc<Shared>,
    ids: Arc<AtomicU64>,
    limit: Arc<Semaphore>,
}

impl Grabber {
    pub fn new(rt: Handle, client: reqwest::Client) -> Self {
        Self {
            rt,
            client,
            shared: Arc::default(),
            ids: Arc::new(AtomicU64::new(1)),
            limit: Arc::new(Semaphore::new(PARALLEL_CHECKS)),
        }
    }

    /// Analiza los enlaces que haya en `text`. Devuelve cuántos enlaces
    /// nuevos se van a comprobar.
    /// Analiza los enlaces de `text`. `batch` marca la tanda del aviso de
    /// enlaces copiados (0 si no hay aviso).
    pub fn analyze(&self, text: &str, batch: u64) -> Result<usize, String> {
        let urls = extract_urls(text);
        if urls.is_empty() {
            return Err("No encuentro ningún enlace http o https en lo que has pegado.".into());
        }
        let mut added = 0;
        for url in urls {
            let name = guess_name(&url);
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
                    size: None,
                    check: Check::Checking,
                    selected: true,
                    batch,
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
        let shared = self.shared.clone();
        let client = self.client.clone();
        let limit = self.limit.clone();
        let ids = self.ids.clone();
        let grabber = self.handle();
        self.rt.spawn(async move {
            let found = {
                let _permit = limit.acquire().await;
                inspect(&client, &url).await
            };
            let mut st = shared.state();
            match found {
                Ok(Found::File(info)) => {
                    let Some(i) = st.items.iter().position(|i| i.id == id) else {
                        return; // lo quitaron mientras tanto
                    };
                    // Un enlace suelto se agrupa por su nombre real.
                    let auto = st
                        .packages
                        .iter()
                        .any(|p| p.id == st.items[i].package && p.auto);
                    if auto {
                        let pkg = st.package_named(&ids, &package_name(&info.filename), true);
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
                Ok(Found::Page {
                    url: page,
                    title,
                    links,
                }) => {
                    let batch = st.items.iter().find(|i| i.id == id).map_or(0, |i| i.batch);
                    st.items.retain(|i| i.id != id);
                    st.drop_empty_packages();
                    if !follow_pages {
                        // Un enlace de una página que resulta ser otra página:
                        // no lo seguimos.
                        return;
                    }
                    if links.is_empty() {
                        shared
                            .notes
                            .lock()
                            .expect("avisos envenenados")
                            .push(format!(
                                "En {} no hay enlaces a archivos.",
                                page.host_str().unwrap_or("esa página")
                            ));
                        return;
                    }
                    let name =
                        title.unwrap_or_else(|| page.host_str().unwrap_or("Página").to_owned());
                    let package = st.package_named(&ids, &name, false);
                    let mut new = Vec::new();
                    for link in links {
                        if st.items.iter().any(|i| i.url == link) {
                            continue;
                        }
                        let id = ids.fetch_add(1, Ordering::Relaxed);
                        st.items.push(Item {
                            id,
                            package,
                            url: link.clone(),
                            name: guess_name(&link),
                            size: None,
                            check: Check::Checking,
                            selected: true,
                            batch,
                        });
                        new.push((id, link));
                    }
                    drop(st);
                    for (id, link) in new {
                        grabber.spawn_check(id, link, false);
                    }
                }
                Err(e) => {
                    if let Some(item) = st.items.iter_mut().find(|i| i.id == id) {
                        item.check = Check::Offline(short_error(&e));
                        item.selected = false;
                    }
                }
            }
        });
    }

    /// Una copia barata para usar dentro de las tareas.
    fn handle(&self) -> Grabber {
        Grabber {
            rt: self.rt.clone(),
            client: self.client.clone(),
            shared: self.shared.clone(),
            ids: self.ids.clone(),
            limit: self.limit.clone(),
        }
    }

    pub fn set_item_selected(&self, id: u64, selected: bool) {
        if let Some(item) = self.shared.state().items.iter_mut().find(|i| i.id == id)
            && !matches!(item.check, Check::Offline(_))
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
            item.selected = selected && !matches!(item.check, Check::Offline(_));
        }
    }

    pub fn select_all(&self, selected: bool) {
        for item in self.shared.state().items.iter_mut() {
            item.selected = selected && !matches!(item.check, Check::Offline(_));
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
        st.items.retain(|i| !matches!(i.check, Check::Offline(_)));
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
        if let Some(p) = self
            .shared
            .state()
            .packages
            .iter_mut()
            .find(|p| p.id == package)
        {
            p.name = name.to_owned();
            p.auto = false;
        }
    }

    pub fn set_package_folder(&self, package: u64, folder: PathBuf) {
        if let Some(p) = self
            .shared
            .state()
            .packages
            .iter_mut()
            .find(|p| p.id == package)
        {
            p.folder = (!folder.as_os_str().is_empty()).then_some(folder);
        }
    }

    pub fn set_package_priority(&self, package: u64, priority: Priority) {
        if let Some(p) = self
            .shared
            .state()
            .packages
            .iter_mut()
            .find(|p| p.id == package)
        {
            p.priority = priority;
        }
    }

    /// Saca del capturador los archivos marcados y los devuelve para
    /// descargarlos. Cada paquete va a la carpeta que diga `dest`, salvo que
    /// se haya elegido otra a mano.
    pub fn take_selected(&self, dest: &Dest) -> Vec<Picked> {
        self.take_where(dest, |i| i.selected)
    }

    /// Saca de la lista lo marcado y disponible de una tanda de enlaces
    /// copiados; lo que aún se comprueba se queda en el capturador.
    pub fn take_batch(&self, batch: u64, dest: &Dest) -> Vec<Picked> {
        self.take_where(dest, |i| {
            i.batch == batch && i.selected && matches!(i.check, Check::Online { .. })
        })
    }

    /// Quita del capturador todos los enlaces de una tanda.
    pub fn discard_batch(&self, batch: u64) {
        let mut st = self.shared.state();
        st.items.retain(|i| i.batch != batch);
        st.drop_empty_packages();
    }

    /// Los archivos de una tanda, el más reciente primero.
    pub fn batch_items(&self, batch: u64) -> Vec<GrabItem> {
        let st = self.shared.state();
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
    pub fn snapshot(&self, dest: &Dest) -> Vec<GrabPackage> {
        let st = self.shared.state();
        st.packages
            .iter()
            .map(|p| GrabPackage {
                id: p.id,
                name: p.name.clone(),
                folder: folder_of(p, &st.items, dest),
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
fn short_error(e: &omni_core::Error) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use omni_core::Network;
    use omni_testserver::{ServerConfig, TestServer, pseudo_random_bytes};
    use std::path::Path;
    use std::time::{Duration, Instant};

    async fn wait_checked(g: &Grabber, n: usize) -> Vec<GrabPackage> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let pkgs = g.snapshot(&dest("/descargas"));
            let items: Vec<_> = pkgs.iter().flat_map(|p| &p.items).collect();
            if items.len() == n && items.iter().all(|i| i.check != Check::Checking) {
                return pkgs;
            }
            assert!(Instant::now() < deadline, "sin terminar: {pkgs:?}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn dest(base: &str) -> Dest {
        Dest {
            base: base.into(),
            per_package: true,
            single_file_too: true,
        }
    }

    #[test]
    fn package_folders() {
        let mut d = dest("/descargas");
        assert_eq!(d.folder("Viaje", 3), Path::new("/descargas/Viaje"));
        assert_eq!(d.folder("peli", 1), Path::new("/descargas/peli"));
        // Nombres que Windows no admite.
        assert_eq!(
            d.folder("Fotos: 2026?", 2),
            Path::new("/descargas/Fotos_ 2026_")
        );
        assert_eq!(d.folder("con", 2), Path::new("/descargas/_con"));
        d.single_file_too = false;
        assert_eq!(d.folder("peli", 1), Path::new("/descargas"));
        assert_eq!(d.folder("Viaje", 3), Path::new("/descargas/Viaje"));
        d.per_package = false;
        assert_eq!(d.folder("Viaje", 3), Path::new("/descargas"));
    }

    #[test]
    fn package_names() {
        assert_eq!(package_name("asturias-2026.part1.rar"), "asturias-2026");
        assert_eq!(package_name("asturias-2026.part12.rar"), "asturias-2026");
        assert_eq!(package_name("copia.7z.001"), "copia");
        assert_eq!(package_name("linux.tar.gz"), "linux");
        assert_eq!(
            package_name("ubuntu-24.04.3-desktop-amd64.iso"),
            "ubuntu-24.04.3-desktop-amd64"
        );
        assert_eq!(package_name("SHA256SUMS"), "SHA256SUMS");
        assert_eq!(package_name(".bashrc"), ".bashrc");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn groups_into_packages_and_picks_selected() {
        let server = TestServer::start(pseudo_random_bytes(5000, 1), ServerConfig::default()).await;
        let net = Network::system_default().unwrap();
        let g = Grabber::new(Handle::current(), net.client().clone());

        let text = format!(
            "{} {} {}",
            server.url("fotos.part1.rar"),
            server.url("fotos.part2.rar"),
            server.url("pelicula.mkv")
        );
        assert_eq!(g.analyze(&text, 0), Ok(3));
        let pkgs = wait_checked(&g, 3).await;
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].name, "fotos");
        assert_eq!(pkgs[0].items.len(), 2);
        assert_eq!(pkgs[0].folder, Path::new("/descargas/fotos"));
        assert_eq!(pkgs[1].name, "pelicula");
        assert_eq!(pkgs[1].items[0].size, Some(5000));
        assert_eq!(pkgs[1].items[0].check, Check::Online { resumable: true });

        // Repetir los mismos enlaces no los duplica.
        assert!(g.analyze(&text, 0).is_err());
        assert!(g.analyze("sin enlaces", 0).is_err());

        // Propiedades del paquete y selección.
        g.rename_package(pkgs[0].id, "Viaje");
        g.set_package_priority(pkgs[0].id, Priority::High);
        g.set_item_selected(pkgs[0].items[1].id, false);
        g.set_package_selected(pkgs[1].id, false);
        let picked = g.take_selected(&dest("/descargas"));
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].name, "fotos.part1.rar");
        assert_eq!(picked[0].package, "Viaje");
        assert_eq!(picked[0].folder, Path::new("/descargas/Viaje"));
        assert_eq!(picked[0].priority, Priority::High);

        // Lo no marcado sigue en el capturador.
        let left = g.snapshot(&dest("/d"));
        assert_eq!(left.iter().map(|p| p.items.len()).sum::<usize>(), 2);
        g.clear();
        assert!(g.snapshot(&dest("/d")).is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn batches_of_copied_links() {
        let server = TestServer::start(pseudo_random_bytes(3000, 2), ServerConfig::default()).await;
        let net = Network::system_default().unwrap();
        let g = Grabber::new(Handle::current(), net.client().clone());

        g.analyze(&server.url("antes.zip"), 0).unwrap();
        let text = format!(
            "{}\n{}\nhttp://127.0.0.1:1/caido.zip",
            server.url("a.zip"),
            server.url("b.zip")
        );
        assert_eq!(g.analyze(&text, 7), Ok(3));
        wait_checked(&g, 4).await;

        let items = g.batch_items(7);
        assert_eq!(items.len(), 3);
        // El más reciente primero.
        assert_eq!(items[0].name, "caido.zip");
        assert!(matches!(items[0].check, Check::Offline(_)));

        // Solo se llevan los disponibles de la tanda.
        let picked = g.take_batch(7, &dest("/d"));
        let mut names: Vec<_> = picked.iter().map(|p| p.name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["a.zip", "b.zip"]);
        assert_eq!(g.batch_items(7).len(), 1);

        g.discard_batch(7);
        assert!(g.batch_items(7).is_empty());
        // Lo que no era de la tanda sigue ahí.
        assert_eq!(g.batch_items(0).len(), 1);
    }
}
