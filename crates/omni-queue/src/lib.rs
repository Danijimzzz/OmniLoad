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

mod external;
mod queue;
mod resolver;
mod schedule;
mod types;

pub use queue::{Queue, QueueConfig};
pub use resolver::{
    CustomDownload, DirectLinks, ExternalJob, ResolveFuture, ResolvedLink, Resolver, RunFuture,
};
pub use schedule::{ALL_DAYS, Schedule, local_now};
pub use types::{ExtractSettings, JobInfo, NewJob, Priority, QueueSettings, State};
