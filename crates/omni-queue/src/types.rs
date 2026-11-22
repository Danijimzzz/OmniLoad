//! Tipos de la cola que ven la interfaz, la CLI y (más adelante) la API de
//! `omnid`. Se pueden serializar.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::schedule::Schedule;

/// En qué punto está una descarga.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Esperando turno en la cola.
    Queued,
    /// Lanzada, aún sondeando el servidor.
    Starting,
    Running,
    /// Se pidió pausar y el motor está guardando el progreso.
    Pausing,
    /// Parada: no arranca hasta que se pida.
    Paused,
    Done,
    Failed,
}

impl State {
    pub fn is_active(self) -> bool {
        matches!(self, State::Starting | State::Running | State::Pausing)
    }

    /// Nombre con el que se guarda en la base de datos.
    pub fn as_str(self) -> &'static str {
        match self {
            State::Queued => "queued",
            State::Starting => "starting",
            State::Running => "running",
            State::Pausing => "pausing",
            State::Paused => "paused",
            State::Done => "done",
            State::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => State::Queued,
            "starting" => State::Starting,
            "running" => State::Running,
            "pausing" => State::Pausing,
            "paused" => State::Paused,
            "done" => State::Done,
            "failed" => State::Failed,
            _ => return None,
        })
    }
}

/// Prioridad en la cola: las altas arrancan antes.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    High,
    #[default]
    Normal,
    Low,
}

impl Priority {
    pub const ALL: [Priority; 3] = [Priority::High, Priority::Normal, Priority::Low];

    pub fn label(self) -> &'static str {
        match self {
            Priority::High => "Alta",
            Priority::Normal => "Normal",
            Priority::Low => "Baja",
        }
    }

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn from_index(i: usize) -> Self {
        Self::ALL.get(i).copied().unwrap_or_default()
    }
}

/// Una descarga nueva para la cola.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NewJob {
    pub url: String,
    pub dir: PathBuf,
    /// Nombre provisional (el real lo confirma el motor).
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub package: String,
    #[serde(default)]
    pub priority: Priority,
    /// Tamaño conocido de antemano (del capturador), hasta que el motor lo
    /// confirme.
    #[serde(default)]
    pub size: Option<u64>,
    /// `true`: a la cola; `false`: se añade parada.
    #[serde(default = "yes")]
    pub start: bool,
    /// Límite de velocidad propio, en bytes por segundo.
    #[serde(default)]
    pub speed_limit: Option<u64>,
}

fn yes() -> bool {
    true
}

impl NewJob {
    pub fn new(url: impl Into<String>, dir: impl Into<PathBuf>) -> Self {
        Self {
            url: url.into(),
            dir: dir.into(),
            name: None,
            package: String::new(),
            priority: Priority::Normal,
            size: None,
            start: true,
            speed_limit: None,
        }
    }
}

/// Ajustes de la cola. Se guardan en la base de datos.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct QueueSettings {
    /// Descargas a la vez (las demás esperan).
    pub max_active: usize,
    /// Conexiones por archivo.
    pub connections: usize,
    /// Límite de velocidad global, en bytes por segundo (`None` = sin
    /// límite).
    pub speed_limit: Option<u64>,
    /// Conexiones como mucho a un mismo servidor sumando todas las
    /// descargas (0 = sin tope).
    pub max_per_server: usize,
    /// Pausa global: no arranca nada de la cola.
    pub held: bool,
    /// Días y horas en que se permite descargar.
    pub schedule: Schedule,
    /// Descomprimir lo descargado.
    pub extract: ExtractSettings,
    /// Interfaces por las que descargar a la vez (vacío = la del sistema).
    pub networks: Vec<String>,
}

/// Qué hacer con los archivos comprimidos que se terminan de descargar.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExtractSettings {
    /// Descomprimir solo al terminar un paquete.
    pub enabled: bool,
    /// Borrar los archivos comprimidos tras descomprimirlos bien.
    pub delete_archives: bool,
    /// Contraseñas a probar, una por elemento.
    pub passwords: Vec<String>,
}

impl Default for ExtractSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            delete_archives: false,
            passwords: Vec::new(),
        }
    }
}

impl Default for QueueSettings {
    fn default() -> Self {
        Self {
            max_active: 3,
            connections: omni_core::DEFAULT_CONNECTIONS_PER_NETWORK,
            speed_limit: None,
            max_per_server: omni_core::DEFAULT_CONNECTIONS_PER_NETWORK,
            held: false,
            schedule: Schedule::default(),
            extract: ExtractSettings::default(),
            networks: Vec::new(),
        }
    }
}

/// Foto de una descarga para pintarla.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobInfo {
    pub id: u64,
    pub package: String,
    pub name: String,
    pub host: String,
    pub folder: PathBuf,
    pub url: String,
    pub state: State,
    pub priority: Priority,
    /// La cola está en pausa global (para decir "En pausa" en vez de "En
    /// espera").
    pub held: bool,
    pub downloaded: u64,
    pub total: Option<u64>,
    /// Bytes por segundo (0 si no está descargando).
    pub rate: f64,
    pub receiving: usize,
    pub connections: usize,
    /// Conexiones que acepta el servidor (0 = sin límite conocido).
    pub server_limit: usize,
    pub retries: u64,
    pub started_once: bool,
    /// Límite de velocidad propio, en bytes por segundo.
    pub speed_limit: Option<u64>,
    pub detail: String,
}

impl JobInfo {
    pub fn fraction(&self) -> Option<f32> {
        if self.state == State::Done {
            return Some(1.0);
        }
        match self.total {
            Some(0) => Some(1.0),
            Some(t) => Some((self.downloaded as f64 / t as f64).clamp(0.0, 1.0) as f32),
            None => None,
        }
    }

    /// Tiempo que falta, si se puede estimar.
    pub fn eta(&self) -> Option<Duration> {
        let t = self.total?;
        (self.rate > 0.0 && t > self.downloaded)
            .then(|| Duration::from_secs_f64((t - self.downloaded) as f64 / self.rate))
    }

    /// Texto corto del estado, como en la columna "Estado".
    pub fn status(&self) -> String {
        match self.state {
            State::Queued if self.held => "En pausa".into(),
            State::Queued => "En espera".into(),
            State::Starting => "Conectando…".into(),
            State::Running => {
                let mut s = format!("Descargando · {} conex.", self.receiving);
                if self.server_limit > 0 && self.server_limit < self.connections {
                    s.push_str(&format!(" (máx. {})", self.server_limit));
                }
                s
            }
            State::Pausing => "Pausando…".into(),
            State::Paused if self.started_once => "Pausada".into(),
            State::Paused => "Sin iniciar".into(),
            State::Done => "Completada".into(),
            State::Failed => match self.detail.as_str() {
                d if d.contains("caducado") => "Enlace caducado".into(),
                _ => "Error".into(),
            },
        }
    }
}
