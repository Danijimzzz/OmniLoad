//! Cuerpos de las peticiones y respuestas de la API.

use omni_queue::{Priority, QueueSettings};
use serde::{Deserialize, Serialize};

/// Estado del servicio.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// Versión de OmniLoad del servicio.
    pub version: String,
    pub pid: u32,
    pub settings: QueueSettings,
}

/// Descargas a las que afecta una orden.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ids {
    pub ids: Vec<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriorityRequest {
    pub ids: Vec<u64>,
    pub priority: Priority,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeedLimitRequest {
    pub ids: Vec<u64>,
    /// Bytes por segundo; `None` = sin límite.
    pub bytes_per_sec: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddResponse {
    pub id: u64,
}

/// Respuesta de error (HTTP 4xx o 5xx).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
}
