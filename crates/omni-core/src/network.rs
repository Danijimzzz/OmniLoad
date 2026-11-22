use std::fmt;
use std::net::IpAddr;
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

    /// Una red atada a una interfaz: todas sus conexiones salen por la
    /// dirección de esa interfaz (por ejemplo, el wifi o el cable).
    pub fn bound(id: NetworkId, iface: &NetInterface) -> Result<Self> {
        let mut builder = default_client_builder().local_address(iface.ip);
        // En Linux y macOS además se ata el socket a la interfaz, para que
        // la ruta no dependa de la tabla de rutas.
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            builder = builder.interface(&iface.name);
        }
        Ok(Self::with_client(id, iface.name.clone(), builder.build()?))
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

/// Una interfaz de red del equipo con la que se puede descargar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetInterface {
    pub name: String,
    pub ip: IpAddr,
}

/// Las interfaces activas con dirección propia (sin la de bucle local ni las
/// de enlace local). Una por interfaz: prefiere IPv4.
pub fn interfaces() -> Vec<NetInterface> {
    let Ok(all) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let mut out: Vec<NetInterface> = Vec::new();
    for i in all {
        let ip = i.ip();
        let usable = match ip {
            IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified(),
            IpAddr::V6(v6) => {
                !v6.is_loopback() && !v6.is_unspecified() && (v6.segments()[0] & 0xffc0) != 0xfe80
            }
        };
        if !usable {
            continue;
        }
        match out.iter_mut().find(|n| n.name == i.name) {
            Some(n) if n.ip.is_ipv6() && ip.is_ipv4() => n.ip = ip,
            Some(_) => {}
            None => out.push(NetInterface { name: i.name, ip }),
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Las redes pedidas por nombre de interfaz. Las que ya no existen se
/// saltan; si no queda ninguna, vacío (= la red del sistema).
pub fn networks_for(names: &[String]) -> Vec<Network> {
    if names.is_empty() {
        return Vec::new();
    }
    let present = interfaces();
    names
        .iter()
        .filter_map(|n| present.iter().find(|i| &i.name == n))
        .enumerate()
        .filter_map(|(k, i)| Network::bound(NetworkId(k as u32), i).ok())
        .collect()
}
