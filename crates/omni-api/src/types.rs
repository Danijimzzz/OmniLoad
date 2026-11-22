//! Cuerpos de las peticiones y respuestas de la API.

use omni_grab::{Dest, GrabPackage, Pick};
use omni_queue::{Priority, QueueSettings};
use serde::{Deserialize, Serialize};

/// Estado del servicio.
/// Quitar descargas; con `files` también se borran sus archivos.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveRequest {
    pub ids: Vec<u64>,
    /// Borrar también el archivo (o lo descargado a medias) y la carpeta si
    /// se queda vacía.
    #[serde(default)]
    pub files: bool,
    /// Carpeta que nunca se borra (la de descargas del usuario).
    #[serde(default)]
    pub keep: Option<std::path::PathBuf>,
}

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

/// Enlaces para el capturador.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddLinksRequest {
    /// Texto con uno o varios enlaces.
    pub text: String,
    /// `true`: forman una tanda nueva (la del aviso de enlaces copiados).
    #[serde(default)]
    pub new_batch: bool,
    /// Si no es tanda nueva, la tanda a la que se suman (0: ninguna).
    #[serde(default)]
    pub batch: u64,
}

/// La lista del capturador.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GrabberView {
    pub packages: Vec<GrabPackage>,
    /// Avisos para el usuario desde la última consulta ("esa página no
    /// tiene archivos"). Se entregan una sola vez.
    pub notes: Vec<String>,
    /// Plugins de hosts instalados.
    #[serde(default)]
    pub hosts: Vec<String>,
}

/// Pasar archivos del capturador a la lista de descargas.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TakeRequest {
    pub pick: Pick,
    /// Carpeta de descargas y si se crea una subcarpeta por paquete (son
    /// ajustes de la ventana).
    pub dest: Dest,
    /// `true`: empiezan a descargarse; `false`: se añaden paradas.
    pub start: bool,
}

/// Lo que pasó al pasar archivos a la lista de descargas.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TakeResponse {
    /// Descargas nuevas.
    pub ids: Vec<u64>,
    /// Archivos que no se pudieron añadir, con el motivo.
    pub errors: Vec<String>,
}
