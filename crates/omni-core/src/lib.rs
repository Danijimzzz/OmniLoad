//! Núcleo de descargas de OmniLoad.
//!
//! El motor reparte un archivo en bloques (8 MiB por defecto) que viven en una
//! cola compartida ([`blocks::BlockQueue`]). Cada conexión coge el siguiente
//! bloque libre al terminar el suyo, así que la conexión más rápida acaba
//! descargando más. Cuando ya no quedan bloques libres, las conexiones ociosas
//! parten por la mitad el bloque pendiente más grande o compiten con él
//! ("final rápido").
//!
//! Cada conexión pertenece a una [`Network`]. En la fase 1 solo existe la red
//! por defecto del sistema, pero la cola y el motor ya trabajan con varias,
//! para que la descarga multi-red (fase 2b) no obligue a rehacer nada.
//!
//! Los datos se escriben directamente en su posición del archivo final
//! (`.part`), sin unir trozos al terminar. El progreso se guarda cada medio
//! segundo en un archivo de estado junto al `.part`, después de un `fsync`, para
//! poder reanudar tras un cierre, un `kill -9` o un corte de luz.
//!
//! El código propio de cada sistema operativo vive en [`platform`].

pub mod blocks;
mod download;
mod error;
pub mod filename;
mod network;
pub mod platform;
mod probe;
mod progress;
mod retry;
mod state;

pub use download::{Destination, Download, DownloadOptions, DownloadOutcome};
pub use error::{Error, Result};
pub use network::{Network, NetworkId};
pub use probe::{RemoteInfo, probe};
pub use progress::{NetworkStats, Progress};
pub use state::state_path_for;

/// Tamaño de bloque por defecto: 8 MiB.
pub const DEFAULT_BLOCK_SIZE: u64 = 8 * 1024 * 1024;
/// Tamaño mínimo de bloque al partir bloques en el final rápido: 1 MiB.
pub const DEFAULT_MIN_BLOCK_SIZE: u64 = 1024 * 1024;
/// Conexiones por red al empezar una descarga.
pub const DEFAULT_CONNECTIONS_PER_NETWORK: usize = 8;
