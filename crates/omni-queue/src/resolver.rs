//! De enlace a descarga.
//!
//! Lo que el usuario añade no siempre es el archivo: un enlace de Mega,
//! Gofile, Google Drive o Mediafire lleva a una página, y el enlace directo
//! (que además caduca) hay que pedírselo al host justo antes de descargar.
//! La cola guarda siempre el enlace original y llama a un [`Resolver`] en cada
//! arranque. Los plugins de hosts de la fase 5 serán resolutores; hoy solo
//! existe [`DirectLinks`], que deja el enlace tal cual.

use std::future::Future;
use std::pin::Pin;

use std::path::PathBuf;

use omni_core::HeaderMap;
use tokio_util::sync::CancellationToken;

/// Lo que hace falta para descargar un enlace.
#[derive(Clone, Debug, Default)]
pub struct ResolvedLink {
    /// Enlace directo al archivo.
    pub url: String,
    /// Cabeceras que pide el host (cookies, tokens…).
    pub headers: HeaderMap,
    /// Nombre del archivo, si el host lo dice (si no, lo decide el motor).
    pub filename: Option<String>,
    /// Si el archivo no se baja por HTTP con el motor (vídeo de yt-dlp,
    /// que descarga y une audio y vídeo por su cuenta), el programa que lo
    /// descarga. `url` se ignora.
    pub external: Option<ExternalJob>,
}

/// Una descarga hecha por un programa externo.
///
/// El programa habla con la cola por su salida estándar, una línea por
/// aviso: `OMNI-PROGRESS <bytes bajados> <total o 0> [<total estimado>]` y, al terminar,
/// `OMNI-FILE <ruta del archivo>`. En `args`, `{dir}` se cambia por la
/// carpeta de destino.
#[derive(Clone, Debug, Default)]
pub struct ExternalJob {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Nombre para los mensajes de error ("yt-dlp").
    pub label: String,
}

/// Futuro que devuelve un [`Resolver`].
pub type ResolveFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ResolvedLink, omni_core::Error>> + Send + 'a>>;

/// Convierte un enlace en uno descargable.
pub trait Resolver: Send + Sync + 'static {
    fn resolve<'a>(&'a self, url: &'a str, cancel: &'a CancellationToken) -> ResolveFuture<'a>;
}

/// Enlaces directos: no hay nada que resolver.
#[derive(Clone, Copy, Debug, Default)]
pub struct DirectLinks;

impl Resolver for DirectLinks {
    fn resolve<'a>(&'a self, url: &'a str, _cancel: &'a CancellationToken) -> ResolveFuture<'a> {
        Box::pin(async move {
            Ok(ResolvedLink {
                url: url.to_owned(),
                ..ResolvedLink::default()
            })
        })
    }
}
