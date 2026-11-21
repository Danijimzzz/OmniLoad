//! Cola de descargas de OmniLoad.
//!
//! Guarda las descargas agrupadas en paquetes, decide cuáles arrancar
//! (prioridad y descargas a la vez), aplica los límites (velocidad global y
//! por descarga, conexiones por servidor) y lo guarda todo en SQLite
//! ([`omni_store`]) para seguir donde se quedó tras cerrar el programa.
//!
//! No sabe nada de ventanas ni de redes locales: la usan la interfaz, la CLI
//! y el servicio `omnid`.
//!
//! Pausar es cancelar: el motor guarda el progreso en `<archivo>.part.omni`
//! y, al reanudar, una descarga nueva con el mismo enlace y carpeta sigue
//! donde se quedó.

mod queue;
mod resolver;
mod types;

pub use queue::{Queue, QueueConfig};
pub use resolver::{DirectLinks, ResolveFuture, ResolvedLink, Resolver};
pub use types::{JobInfo, NewJob, Priority, QueueSettings, State};
