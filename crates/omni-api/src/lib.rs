//! API local de OmniLoad.
//!
//! `omnid` es el servicio que descarga en segundo plano: tiene la cola
//! ([`omni_queue`]) y la ofrece por HTTP en `127.0.0.1`, en un puerto libre.
//! La ventana y la CLI son clientes. Así las descargas no dependen de que la
//! ventana esté abierta, y la CLI ve y controla lo mismo que la ventana.
//!
//! - Al arrancar, el servicio escribe `omnid.json` (puerto y token) junto a
//!   la base de datos; los clientes lo leen para encontrarlo. Solo el
//!   usuario puede leer ese archivo, y cada petición lleva el token, así que
//!   ni otra web abierta en el navegador ni otro usuario del equipo pueden
//!   mandar órdenes.
//! - Un candado (`omnid.lock`) impide que haya dos servicios con la misma
//!   base de datos.
//! - Todas las rutas van bajo `/api/v1`, para poder cambiar la API sin
//!   romper clientes viejos.
//!
//! [`Service`] es el servicio (lo usa `omnid`, y la ventana si no encuentra
//! el programa `omnid`), [`Client`] el cliente asíncrono y [`RemoteQueue`] un
//! cliente pensado para la ventana, que no puede esperar a la red en su hilo.

mod client;
mod launch;
mod paths;
mod remote;
mod server;
mod types;

pub use client::{Client, ClientError};
pub use launch::{connect_or_launch, omnid_exe, spawn_omnid};
pub use paths::{Endpoint, Paths};
pub use remote::RemoteQueue;
pub use server::{Service, ServiceError};
pub use types::{AddResponse, ErrorResponse, Ids, PriorityRequest, SpeedLimitRequest, Status};

/// Versión de la API (va en la ruta: `/api/v1`).
pub const API_VERSION: u32 = 1;
