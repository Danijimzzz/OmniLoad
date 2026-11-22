//! Plugins de hosts de descarga.
//!
//! Un enlace de Pixeldrain, Archive.org, Dropbox o Mediafire no es el archivo:
//! hay que preguntar a su API o leer su página. Cada host es un [`Host`] que
//! sabe dos cosas con el mismo enlace:
//!
//! - **comprobarlo** para el capturador (nombre, tamaño, o la lista de
//!   archivos si es una carpeta), y
//! - **resolverlo** justo antes de descargar, porque el enlace directo suele
//!   caducar o depender de cookies.
//!
//! [`Hosts`] junta los plugins y los entrega como [`Checkers`] (capturador) y
//! como [`Resolver`] (cola). Los enlaces que ningún plugin reconoce siguen
//! por el camino genérico: son enlaces directos.
//!
//! Un plugin solo recibe el cliente HTTP de la red por la que se comprueba,
//! así que no sabe nada de redes ni de la cola: la descarga multi-red de la
//! última fase no lo toca.

mod archive;
mod dropbox;
mod gdrive;
mod gofile;
mod html;
mod mediafire;
mod pixeldrain;

use std::sync::Arc;

use omni_core::Error;
use omni_grab::{CheckFuture, Checkers, HostChecker};
use omni_queue::{ResolveFuture, ResolvedLink, Resolver};
use reqwest::StatusCode;
use tokio_util::sync::CancellationToken;
use url::Url;

pub use archive::ArchiveOrg;
pub use dropbox::Dropbox;
pub use gdrive::GoogleDrive;
pub use gofile::Gofile;
pub use mediafire::Mediafire;
pub use pixeldrain::Pixeldrain;

/// Un host de descarga.
pub trait Host: Send + Sync + 'static {
    /// Nombre que se enseña en la columna "Host".
    fn name(&self) -> &str;

    /// ¿Es un enlace de este host?
    fn handles(&self, url: &Url) -> bool;

    /// Cuántos enlaces de este host se comprueban a la vez.
    fn parallel(&self) -> usize {
        4
    }

    /// Qué hay detrás del enlace: un archivo, una carpeta o un error.
    fn check<'a>(&'a self, client: &'a reqwest::Client, url: &'a Url) -> CheckFuture<'a>;

    /// El enlace directo y lo que haga falta para descargarlo (cabeceras,
    /// nombre). Se llama en cada arranque de la descarga.
    fn resolve<'a>(&'a self, client: &'a reqwest::Client, url: &'a Url) -> ResolveFuture<'a>;
}

/// Los plugins instalados, en orden de preferencia.
#[derive(Clone, Default)]
pub struct Hosts {
    list: Vec<Arc<dyn Host>>,
}

impl Hosts {
    /// Sin plugins: todo son enlaces directos.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Los plugins que vienen con OmniLoad.
    pub fn standard() -> Self {
        Self::empty()
            .with(Pixeldrain::new())
            .with(ArchiveOrg::new())
            .with(Dropbox::new())
            .with(Mediafire::new())
            .with(GoogleDrive::new())
            .with(Gofile::new())
    }

    /// Añade un plugin. Los añadidos antes tienen preferencia.
    pub fn with(mut self, host: impl Host) -> Self {
        self.list.push(Arc::new(host));
        self
    }

    /// Nombres de los plugins instalados.
    pub fn names(&self) -> Vec<String> {
        self.list.iter().map(|h| h.name().to_owned()).collect()
    }

    /// El plugin que reconoce el enlace, si hay alguno.
    pub fn find(&self, url: &Url) -> Option<&Arc<dyn Host>> {
        self.list.iter().find(|h| h.handles(url))
    }

    /// Los plugins para el capturador.
    pub fn checkers(&self) -> Checkers {
        self.list
            .iter()
            .fold(Checkers::new(), |c, h| c.with(AsChecker(h.clone())))
    }

    /// El resolutor para la cola. `client` es el de la red por la que se
    /// piden los enlaces.
    pub fn resolver(&self, client: reqwest::Client) -> HostResolver {
        HostResolver {
            hosts: self.clone(),
            client,
        }
    }
}

struct AsChecker(Arc<dyn Host>);

impl HostChecker for AsChecker {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn handles(&self, url: &Url) -> bool {
        self.0.handles(url)
    }
    fn parallel(&self) -> usize {
        self.0.parallel()
    }
    fn check<'a>(&'a self, client: &'a reqwest::Client, url: &'a Url) -> CheckFuture<'a> {
        self.0.check(client, url)
    }
}

/// Pide el enlace directo al plugin del host; si ninguno reconoce el enlace,
/// lo deja tal cual.
#[derive(Clone)]
pub struct HostResolver {
    hosts: Hosts,
    client: reqwest::Client,
}

impl Resolver for HostResolver {
    fn resolve<'a>(&'a self, url: &'a str, cancel: &'a CancellationToken) -> ResolveFuture<'a> {
        Box::pin(async move {
            let parsed = Url::parse(url).map_err(|e| Error::InvalidUrl(format!("{url}: {e}")))?;
            let Some(host) = self.hosts.find(&parsed) else {
                return Ok(ResolvedLink {
                    url: url.to_owned(),
                    ..ResolvedLink::default()
                });
            };
            tokio::select! {
                _ = cancel.cancelled() => Err(Error::Cancelled),
                r = host.resolve(&self.client, &parsed) => r,
            }
        })
    }
}

/// Un enlace directo ya resuelto.
pub(crate) fn direct(url: impl Into<String>) -> ResolvedLink {
    ResolvedLink {
        url: url.into(),
        ..ResolvedLink::default()
    }
}

/// ¿Es `host` el dominio `domain` o un subdominio suyo?
pub(crate) fn on_domain(url: &Url, domain: &str) -> bool {
    url.host_str().is_some_and(|h| {
        let h = h.to_ascii_lowercase();
        h == domain || h.strip_suffix(domain).is_some_and(|p| p.ends_with('.'))
    })
}

/// Segmentos de la ruta sin vacíos.
pub(crate) fn segments(url: &Url) -> Vec<String> {
    url.path_segments()
        .map(|s| {
            s.filter(|p| !p.is_empty())
                .map(|p| {
                    percent_encoding::percent_decode_str(p)
                        .decode_utf8_lossy()
                        .into_owned()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Como mucho se leen estos bytes de una respuesta de API o de una página.
const MAX_BODY: usize = 8 * 1024 * 1024;

/// Descarga un cuerpo de texto de hasta [`MAX_BODY`] bytes.
pub(crate) async fn fetch_text(
    client: &reqwest::Client,
    url: Url,
) -> omni_core::Result<(StatusCode, String)> {
    let mut resp = client.get(url).send().await?;
    let status = resp.status();
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        let room = MAX_BODY - buf.len();
        buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if buf.len() >= MAX_BODY {
            break;
        }
    }
    Ok((status, String::from_utf8_lossy(&buf).into_owned()))
}

/// Pide un JSON a la API de un host.
pub(crate) async fn fetch_json(
    client: &reqwest::Client,
    url: Url,
) -> omni_core::Result<(StatusCode, serde_json::Value)> {
    let (status, text) = fetch_text(client, url).await?;
    let json = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    Ok((status, json))
}

/// Un código HTTP inesperado de la API de un host, como error.
pub(crate) fn status_error(status: StatusCode) -> Error {
    match status.as_u16() {
        s @ (403 | 404 | 410) => Error::LinkExpired { status: s },
        s => Error::HttpStatus { status: s },
    }
}

/// Un tamaño que la API da como número o como texto.
pub(crate) fn json_size(v: &serde_json::Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str()?.trim().parse().ok())
}

/// Lo que hay tras un enlace que puede ser un archivo o una página.
pub(crate) enum Probed {
    File(omni_core::RemoteInfo),
    /// Una página HTML (con su dirección final).
    Page {
        url: Url,
        html: String,
    },
}

/// Como el sondeo del motor, pero si el servidor contesta con una página
/// HTML la devuelve en vez de dar error (formularios de confirmación,
/// avisos de cuota…).
pub(crate) async fn probe_or_page(
    client: &reqwest::Client,
    url: &Url,
    headers: &reqwest::header::HeaderMap,
) -> omni_core::Result<Probed> {
    use reqwest::header::{CONTENT_TYPE, RANGE};
    let resp = client
        .get(url.clone())
        .headers(headers.clone())
        .header(RANGE, "bytes=0-0")
        .send()
        .await?;
    let status = resp.status();
    let is_html = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.to_ascii_lowercase().starts_with("text/html"));
    let final_url = resp.url().clone();
    if is_html && (status.is_success() || status.as_u16() == 403 || status.as_u16() == 404) {
        let mut resp = resp;
        let mut buf = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            let room = MAX_BODY - buf.len();
            buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
            if buf.len() >= MAX_BODY {
                break;
            }
        }
        if status.as_u16() == 404 {
            return Err(Error::LinkExpired { status: 404 });
        }
        return Ok(Probed::Page {
            url: final_url,
            html: String::from_utf8_lossy(&buf).into_owned(),
        });
    }
    let headers = resp.headers().clone();
    drop(resp);
    omni_core::info_from_response(status, final_url, &headers).map(Probed::File)
}
