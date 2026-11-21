use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::Result;

/// Identificador de una red dentro de una descarga.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NetworkId(pub u32);

impl fmt::Display for NetworkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "red#{}", self.0)
    }
}

/// Una salida a internet por la que se abren conexiones.
///
/// En la fase 1 solo existe [`Network::system_default`], que deja al sistema
/// operativo elegir la ruta. En la última fase del plan habrá redes atadas a una interfaz
/// concreta (wifi, cable, móvil por USB). Lo propio de cada sistema operativo
/// para atar sockets (`SO_BINDTODEVICE` en Linux, `IP_BOUND_IF` en macOS,
/// `IP_UNICAST_IF` en Windows) irá en [`crate::platform`] y se usará al
/// construir el cliente HTTP de cada red; el resto del motor no cambia.
#[derive(Clone)]
pub struct Network {
    id: NetworkId,
    name: String,
    client: reqwest::Client,
}

impl Network {
    /// La red por defecto del sistema.
    pub fn system_default() -> Result<Self> {
        Ok(Self::with_client(
            NetworkId(0),
            "sistema",
            default_client_builder().build()?,
        ))
    }

    /// Una red con un cliente HTTP ya construido (útil en pruebas).
    pub fn with_client(id: NetworkId, name: impl Into<String>, client: reqwest::Client) -> Self {
        Self {
            id,
            name: name.into(),
            client,
        }
    }

    pub fn id(&self) -> NetworkId {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }
}

impl fmt::Debug for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Network")
            .field("id", &self.id)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Configuración común de los clientes HTTP del motor. Sin descompresión
/// automática: necesitamos los bytes tal cual para que los rangos cuadren.
///
/// Solo HTTP/1.1: con HTTP/2 el cliente mete todas las peticiones de bloques
/// en una única conexión TCP (varios flujos multiplexados), así que no hay
/// paralelismo real, y muchos servidores cortan los flujos de más. Cada
/// conexión del motor tiene que ser una conexión TCP propia.
pub fn default_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .http1_only()
        .user_agent(concat!("OmniLoad/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30))
}
