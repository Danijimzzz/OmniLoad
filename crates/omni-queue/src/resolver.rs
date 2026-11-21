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
