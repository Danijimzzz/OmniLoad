//! Quién comprueba cada enlace.
//!
//! Un enlace normal se comprueba con una petición HTTP ([`inspect`]). Los
//! hosts como Mega, Gofile, Google Drive o Mediafire no enseñan el archivo
//! así: hay que preguntar a su API o leer su página. Cada uno tendrá su
//! plugin (fase 5), que implementa [`HostChecker`] para el capturador y
//! [`omni_queue::Resolver`] para la descarga. [`Checkers`] elige el plugin
//! que reconoce el enlace o, si ninguno lo hace, la comprobación genérica.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use url::Url;

use crate::inspect::{Found, inspect};

/// Futuro que devuelve [`HostChecker::check`].
pub type CheckFuture<'a> = Pin<Box<dyn Future<Output = omni_core::Result<Found>> + Send + 'a>>;

/// Comprobaciones a la vez contra un mismo servidor, si el plugin no dice
/// otra cosa. Muchos hosts cortan a quien abre demasiadas conexiones.
pub const DEFAULT_PER_HOST: usize = 8;

/// Sabe comprobar los enlaces de un host.
pub trait HostChecker: Send + Sync + 'static {
    /// Nombre que se enseña en la columna "Host" ("Mega", "Gofile"…).
    fn name(&self) -> &str;

    /// ¿Es un enlace de este host?
    fn handles(&self, url: &Url) -> bool;

    /// Cuántos enlaces de este host se comprueban a la vez.
    fn parallel(&self) -> usize {
        DEFAULT_PER_HOST
    }

    /// Mira qué hay detrás del enlace: un archivo (nombre, tamaño), una
    /// carpeta con sus archivos o un error si ya no existe. `client` es el
    /// de la red por la que se comprueba.
    fn check<'a>(&'a self, client: &'a reqwest::Client, url: &'a Url) -> CheckFuture<'a>;
}

/// Los plugins de hosts conocidos, en orden de preferencia.
#[derive(Clone, Default)]
pub struct Checkers {
    hosts: Vec<Arc<dyn HostChecker>>,
}

/// Quién comprobará un enlace.
pub(crate) struct Route {
    /// Plugin, o `None` para la comprobación genérica.
    pub checker: Option<Arc<dyn HostChecker>>,
    /// Lo que se enseña en la columna "Host".
    pub label: String,
    /// Clave para repartir las comprobaciones por servidor.
    pub key: String,
    pub parallel: usize,
}

impl Checkers {
    pub fn new() -> Self {
        Self::default()
    }

    /// Añade un plugin. Los añadidos antes tienen preferencia.
    pub fn with(mut self, checker: impl HostChecker) -> Self {
        self.hosts.push(Arc::new(checker));
        self
    }

    /// Nombres de los plugins instalados.
    pub fn names(&self) -> Vec<String> {
        self.hosts.iter().map(|h| h.name().to_owned()).collect()
    }

    pub(crate) fn route(&self, url: &Url) -> Route {
        match self.hosts.iter().find(|h| h.handles(url)) {
            Some(h) => Route {
                label: h.name().to_owned(),
                key: format!("plugin:{}", h.name()),
                parallel: h.parallel().max(1),
                checker: Some(h.clone()),
            },
            None => {
                let host = url.host_str().unwrap_or("").to_owned();
                Route {
                    label: host.clone(),
                    key: host,
                    parallel: DEFAULT_PER_HOST,
                    checker: None,
                }
            }
        }
    }

    /// Comprueba un enlace con el plugin que lo reconozca o, si no, con una
    /// petición HTTP.
    pub async fn check(&self, client: &reqwest::Client, url: &Url) -> omni_core::Result<Found> {
        match self.route(url).checker {
            Some(c) => c.check(client, url).await,
            None => inspect(client, url).await,
        }
    }
}
