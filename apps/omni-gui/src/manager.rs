//! Cola de descargas de la ventana: la de `omni-queue`, con la lista
//! guardada en SQLite. Aquí solo quedan los nombres que usa la interfaz y
//! algún texto para pintar.

use std::sync::Arc;

use omni_queue::QueueConfig;
use omni_store::Store;
use tokio::runtime::Handle;
use tracing::warn;

use crate::format;

pub use omni_queue::{
    JobInfo as RowInfo, NewJob as NewDownload, Priority, Queue as Manager, State,
};

/// Abre la cola con la base de datos de siempre. Si no se puede abrir (disco
/// lleno, permisos…), la cola funciona igual pero sin guardar nada.
pub fn open(rt: Handle) -> Manager {
    let store = match Store::default_path().map(|p| (Store::open(&p), p)) {
        Some((Ok(s), _)) => s,
        Some((Err(e), p)) => {
            warn!(
                "no se pudo abrir {}: {e}; la lista no se guardará",
                p.display()
            );
            Store::open_in_memory().expect("SQLite en memoria")
        }
        None => Store::open_in_memory().expect("SQLite en memoria"),
    };
    let cfg = QueueConfig::new(Arc::new(store));
    match Manager::open(rt.clone(), cfg) {
        Ok(q) => q,
        Err(e) => {
            // Base de datos ilegible: se empieza con una vacía en memoria.
            warn!("no se pudo leer la lista de descargas: {e}");
            let store = Arc::new(Store::open_in_memory().expect("SQLite en memoria"));
            Manager::open(rt, QueueConfig::new(store)).expect("cola en memoria")
        }
    }
}

/// Texto "a de b" para el tamaño.
pub fn sizes(r: &RowInfo) -> String {
    match r.total {
        Some(t) => format::bytes(t),
        None if r.downloaded > 0 => format::bytes(r.downloaded),
        None => String::new(),
    }
}
