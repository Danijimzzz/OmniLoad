//! Capturador de enlaces de la ventana: analiza lo que se pega, enseña los
//! archivos encontrados y deja elegir cuáles descargar.
//!
//! Igual que [`crate::manager`], no sabe nada de Slint: la interfaz pide una
//! foto ([`GrabRow`]) cuatro veces por segundo.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use omni_grab::{Found, extract_urls, inspect};
use tokio::runtime::Handle;
use tokio::sync::Semaphore;
use url::Url;

use crate::format;

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

/// Lo que la interfaz necesita para pintar un archivo encontrado.
#[derive(Clone, Debug, PartialEq)]
pub struct GrabRow {
    pub name: String,
    pub size: String,
    /// Tamaño en bytes, si se conoce.
    pub bytes: Option<u64>,
    pub source: String,
    pub status: String,
    pub check: Check,
    pub selected: bool,
}

/// Un archivo elegido para descargar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picked {
    pub url: String,
    pub name: String,
}

struct Item {
    id: u64,
    url: Url,
    name: String,
    size: Option<u64>,
    /// De dónde salió: el enlace pegado o la página donde estaba.
    source: String,
    check: Check,
    selected: bool,
}

#[derive(Default)]
struct Shared {
    items: Mutex<Vec<Item>>,
    /// Avisos para el usuario ("esa página no tiene archivos").
    notes: Mutex<Vec<String>>,
}

impl Shared {
    fn items(&self) -> MutexGuard<'_, Vec<Item>> {
        self.items.lock().expect("capturador envenenado")
    }

    fn update(&self, id: u64, f: impl FnOnce(&mut Item)) {
        if let Some(item) = self.items().iter_mut().find(|i| i.id == id) {
            f(item);
        }
    }

    fn remove(&self, id: u64) {
        self.items().retain(|i| i.id != id);
    }

    /// Añade un enlace si no está ya; devuelve su id.
    fn push(&self, ids: &AtomicU64, url: Url, source: String) -> Option<u64> {
        let mut items = self.items();
        if items.iter().any(|i| i.url == url) {
            return None;
        }
        let id = ids.fetch_add(1, Ordering::Relaxed);
        let name = omni_core::filename::from_url(&url)
            .unwrap_or_else(|| url.host_str().unwrap_or("archivo").to_owned());
        items.push(Item {
            id,
            url,
            name,
            size: None,
            source,
            check: Check::Checking,
            selected: true,
        });
        Some(id)
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
            ids: Arc::default(),
            limit: Arc::new(Semaphore::new(PARALLEL_CHECKS)),
        }
    }

    /// Analiza los enlaces que haya en `text`. Devuelve cuántos enlaces
    /// nuevos se van a comprobar.
    pub fn analyze(&self, text: &str) -> Result<usize, String> {
        let urls = extract_urls(text);
        if urls.is_empty() {
            return Err("No encuentro ningún enlace http o https en lo que has pegado.".into());
        }
        let mut added = 0;
        for url in urls {
            let source = short(&url);
            if let Some(id) = self.shared.push(&self.ids, url.clone(), source) {
                added += 1;
                self.spawn_check(id, url, true);
            }
        }
        if added == 0 {
            return Err("Esos enlaces ya están en la lista.".into());
        }
        Ok(added)
    }

    /// Comprueba un enlace en segundo plano. Si es una página y
    /// `follow_pages`, sus enlaces a archivos pasan a la lista en su lugar.
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
            match found {
                Ok(Found::File(info)) => shared.update(id, |i| {
                    i.name = info.filename.clone();
                    i.size = info.size;
                    i.check = Check::Online {
                        resumable: info.is_segmentable(),
                    };
                }),
                Ok(Found::Page { url: page, links }) => {
                    shared.remove(id);
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
                            .push(format!("En {} no hay enlaces a archivos.", short(&page)));
                        return;
                    }
                    let source = short(&page);
                    for link in links {
                        if let Some(id) = shared.push(&ids, link.clone(), source.clone()) {
                            grabber.spawn_check(id, link, false);
                        }
                    }
                }
                Err(e) => shared.update(id, |i| {
                    i.check = Check::Offline(short_error(&e));
                    i.selected = false;
                }),
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

    pub fn set_selected(&self, i: usize, selected: bool) {
        if let Some(item) = self.shared.items().get_mut(i)
            && !matches!(item.check, Check::Offline(_))
        {
            item.selected = selected;
        }
    }

    pub fn select_all(&self, selected: bool) {
        for item in self.shared.items().iter_mut() {
            item.selected = selected && !matches!(item.check, Check::Offline(_));
        }
    }

    pub fn remove(&self, i: usize) {
        let mut items = self.shared.items();
        if i < items.len() {
            items.remove(i);
        }
    }

    pub fn clear(&self) {
        self.shared.items().clear();
    }

    /// Saca de la lista los seleccionados y los devuelve para descargarlos.
    pub fn take_selected(&self) -> Vec<Picked> {
        let mut items = self.shared.items();
        let (picked, rest): (Vec<Item>, Vec<Item>) = items.drain(..).partition(|i| i.selected);
        *items = rest;
        picked
            .into_iter()
            .map(|i| Picked {
                url: i.url.into(),
                name: i.name,
            })
            .collect()
    }

    /// Avisos pendientes; se vacían al leerlos.
    pub fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut *self.shared.notes.lock().expect("avisos envenenados"))
    }

    pub fn snapshot(&self) -> Vec<GrabRow> {
        self.shared
            .items()
            .iter()
            .map(|i| GrabRow {
                name: i.name.clone(),
                size: i.size.map(format::bytes).unwrap_or_default(),
                bytes: i.size,
                source: i.source.clone(),
                status: match &i.check {
                    Check::Checking => "Comprobando…".into(),
                    Check::Online { resumable: true } => "Disponible".into(),
                    Check::Online { resumable: false } => {
                        "Disponible (una conexión, sin reanudar)".into()
                    }
                    Check::Offline(e) => format!("No disponible: {e}"),
                },
                check: i.check.clone(),
                selected: i.selected,
            })
            .collect()
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

/// "ejemplo.com/descargas/" para mostrar de dónde salió un enlace.
fn short(url: &Url) -> String {
    let host = url.host_str().unwrap_or("");
    let path = url.path();
    let dir = &path[..path.rfind('/').map_or(0, |p| p + 1)];
    format!("{host}{dir}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use omni_core::Network;
    use omni_testserver::{ServerConfig, TestServer, pseudo_random_bytes};
    use std::time::{Duration, Instant};

    async fn wait_checked(g: &Grabber, n: usize) -> Vec<GrabRow> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let rows = g.snapshot();
            if rows.len() == n && rows.iter().all(|r| r.check != Check::Checking) {
                return rows;
            }
            assert!(Instant::now() < deadline, "sin terminar: {rows:?}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shows_files_before_downloading() {
        let server = TestServer::start(pseudo_random_bytes(5000, 1), ServerConfig::default()).await;
        let net = Network::system_default().unwrap();
        let g = Grabber::new(Handle::current(), net.client().clone());

        let text = format!(
            "{} y {}",
            server.url("pelicula.mkv"),
            server.url("subtitulos.srt")
        );
        assert_eq!(g.analyze(&text), Ok(2));
        let rows = wait_checked(&g, 2).await;
        assert_eq!(rows[0].name, "pelicula.mkv");
        assert_eq!(rows[0].size, "4,9 KiB");
        assert_eq!(rows[0].check, Check::Online { resumable: true });
        assert!(rows.iter().all(|r| r.selected));

        // Repetir los mismos enlaces no los duplica.
        assert!(g.analyze(&text).is_err());
        assert!(g.analyze("sin enlaces").is_err());

        // Solo se descarga lo marcado.
        g.set_selected(1, false);
        let picked = g.take_selected();
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].name, "pelicula.mkv");
        assert_eq!(g.snapshot().len(), 1);
        g.clear();
        assert!(g.snapshot().is_empty());
    }

    #[test]
    fn short_source() {
        let u = Url::parse("https://ejemplo.com/a/b/c.zip?x=1").unwrap();
        assert_eq!(short(&u), "ejemplo.com/a/b/");
    }
}
