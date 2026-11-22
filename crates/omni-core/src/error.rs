use std::io;

/// Resultado con el error del núcleo.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Errores del motor de descargas.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("URL no válida: {0}")]
    InvalidUrl(String),

    #[error("error de red: {}", chain(.0))]
    Network(#[from] reqwest::Error),

    #[error("el servidor respondió HTTP {status}")]
    HttpStatus { status: u16 },

    /// 403, 404 o 410: el enlace ya no sirve. En la fase 5 esto pedirá al
    /// plugin del host que lo renueve.
    #[error("el enlace ha caducado o ya no existe (HTTP {status})")]
    LinkExpired { status: u16 },

    #[error("el servidor no respetó el rango pedido")]
    RangeNotHonoured,

    #[error("el archivo cambió en el servidor durante la descarga")]
    RemoteChanged,

    #[error("la respuesta terminó antes de tiempo ({received} de {expected} bytes)")]
    ShortBody { received: u64, expected: u64 },

    #[error("error de disco: {0}")]
    Io(#[from] io::Error),

    #[error("el archivo de destino ya existe: {0}")]
    AlreadyExists(String),

    #[error("el tamaño final ({actual} bytes) no coincide con el esperado ({expected} bytes)")]
    SizeMismatch { expected: u64, actual: u64 },

    #[error("el SHA-256 no coincide: se esperaba {expected} y se obtuvo {actual}")]
    HashMismatch { expected: String, actual: String },

    #[error("se agotaron los reintentos; último error: {0}")]
    RetriesExhausted(Box<Error>),

    #[error("descarga cancelada")]
    Cancelled,

    /// Un host (Pixeldrain, Mediafire…) no deja descargar y sabe decir por
    /// qué: archivo borrado, cuota agotada, enlace privado…
    #[error("{host}: {message}")]
    Host { host: String, message: String },
}

impl Error {
    /// Un error explicable de un host.
    pub fn host(host: impl Into<String>, message: impl Into<String>) -> Self {
        Error::Host {
            host: host.into(),
            message: message.into(),
        }
    }

    /// ¿Merece la pena reintentar? Cortes de red, 5xx, 429 y respuestas
    /// incompletas sí; enlaces caducados, errores de disco o cambios en el
    /// archivo remoto no.
    pub fn is_retriable(&self) -> bool {
        match self {
            Error::Network(e) => !e.is_builder() && !e.is_redirect(),
            Error::HttpStatus { status } => *status == 408 || *status == 429 || *status >= 500,
            Error::ShortBody { .. } => true,
            _ => false,
        }
    }
}

/// Mensaje de un error con todas sus causas ("a: b: c"). reqwest esconde la
/// causa real (DNS, TLS, conexión rechazada) en la cadena de `source()`.
fn chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(s) = cur {
        let msg = s.to_string();
        if !out.contains(&msg) {
            out.push_str(": ");
            out.push_str(&msg);
        }
        cur = s.source();
    }
    out
}
