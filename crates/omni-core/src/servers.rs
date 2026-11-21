//! Conexiones por servidor, compartidas entre descargas.
//!
//! Los servidores limitan las conexiones por IP, así que dos descargas al
//! mismo servidor tienen que repartirse el mismo cupo. [`ServerLimits`] guarda
//! un límite adaptativo por servidor y red (ver `limiter`) y el tope que elige
//! el usuario. Lo aprendido (cuántas conexiones acepta cada servidor) se puede
//! sacar con [`ServerLimits::learned`] para guardarlo y devolverlo en la
//! siguiente sesión con [`ServerLimits::remember`].

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::limiter::{Limiter, REGROW_AFTER};

/// Cuántas conexiones acepta un servidor por una red.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerLimit {
    pub host: String,
    /// Nombre de la red ([`crate::Network::name`]).
    pub network: String,
    pub max: usize,
}

/// Estado actual de un servidor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerStatus {
    pub host: String,
    pub network: String,
    /// Conexiones abiertas ahora mismo (de todas las descargas).
    pub active: usize,
    /// Conexiones permitidas ahora mismo.
    pub allowed: usize,
    /// Máximo que acepta el servidor, si se sabe.
    pub learned: Option<usize>,
}

type Key = (String, String);

/// Límites de conexiones por servidor, compartidos por todas las descargas.
pub struct ServerLimits {
    /// Tope del usuario; 0 = sin tope.
    max_per_server: AtomicUsize,
    limiters: Mutex<HashMap<Key, Arc<Limiter>>>,
    known: Mutex<HashMap<Key, usize>>,
}

impl fmt::Debug for ServerLimits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerLimits")
            .field("max_per_server", &self.max_per_server())
            .finish_non_exhaustive()
    }
}

impl Default for ServerLimits {
    fn default() -> Self {
        Self::new(0)
    }
}

impl ServerLimits {
    /// `max_per_server`: conexiones como mucho a un mismo servidor sumando
    /// todas las descargas (0 = sin tope).
    pub fn new(max_per_server: usize) -> Self {
        Self {
            max_per_server: AtomicUsize::new(max_per_server),
            limiters: Mutex::default(),
            known: Mutex::default(),
        }
    }

    fn cap(&self) -> usize {
        match self.max_per_server() {
            0 => usize::MAX / 2,
            n => n,
        }
    }

    pub fn max_per_server(&self) -> usize {
        self.max_per_server.load(Ordering::Relaxed)
    }

    /// Cambia el tope; vale también para las descargas en marcha (las
    /// conexiones de más terminan su bloque y esperan).
    pub fn set_max_per_server(&self, n: usize) {
        self.max_per_server.store(n, Ordering::Relaxed);
        let cap = self.cap();
        for l in self
            .limiters
            .lock()
            .expect("servidores envenenado")
            .values()
        {
            l.set_cap(cap);
        }
    }

    /// Recuerda cuántas conexiones acepta un servidor (de otra sesión). Solo
    /// afecta a los servidores con los que aún no se ha hablado.
    pub fn remember(&self, limit: ServerLimit) {
        self.known
            .lock()
            .expect("servidores envenenado")
            .insert((limit.host, limit.network), limit.max.max(1));
    }

    /// Lo aprendido hasta ahora: servidores que rechazaron conexiones de más.
    pub fn learned(&self) -> Vec<ServerLimit> {
        let mut out: HashMap<Key, usize> =
            self.known.lock().expect("servidores envenenado").clone();
        for (k, l) in self.limiters.lock().expect("servidores envenenado").iter() {
            if let Some(max) = l.learned() {
                out.insert(k.clone(), max);
            }
        }
        let mut v: Vec<_> = out
            .into_iter()
            .map(|((host, network), max)| ServerLimit { host, network, max })
            .collect();
        v.sort_by(|a, b| (&a.host, &a.network).cmp(&(&b.host, &b.network)));
        v
    }

    /// Servidores con los que se ha hablado en esta sesión.
    pub fn status(&self) -> Vec<ServerStatus> {
        let mut v: Vec<_> = self
            .limiters
            .lock()
            .expect("servidores envenenado")
            .iter()
            .map(|((host, network), l)| ServerStatus {
                host: host.clone(),
                network: network.clone(),
                active: l.active(),
                allowed: l.allowed(),
                learned: l.learned(),
            })
            .collect();
        v.sort_by(|a, b| (&a.host, &a.network).cmp(&(&b.host, &b.network)));
        v
    }

    /// Límite compartido de un servidor por una red.
    pub(crate) fn limiter(&self, host: &str, network: &str) -> Arc<Limiter> {
        let key = (host.to_ascii_lowercase(), network.to_owned());
        let mut map = self.limiters.lock().expect("servidores envenenado");
        if let Some(l) = map.get(&key) {
            return l.clone();
        }
        let cap = self.cap();
        let known = self
            .known
            .lock()
            .expect("servidores envenenado")
            .get(&key)
            .copied();
        let l = Arc::new(match known {
            Some(k) => Limiter::with_known(cap, k, REGROW_AFTER),
            None => Limiter::new(cap, REGROW_AFTER),
        });
        map.insert(key, l.clone());
        l
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_server_shares_the_limit() {
        let s = ServerLimits::new(4);
        let a = s.limiter("Ejemplo.com", "sistema");
        let b = s.limiter("ejemplo.com", "sistema");
        assert!(Arc::ptr_eq(&a, &b));
        let other_net = s.limiter("ejemplo.com", "wifi");
        assert!(!Arc::ptr_eq(&a, &other_net));
        assert_eq!(a.allowed(), 4);
        s.set_max_per_server(2);
        assert_eq!(a.allowed(), 2);
        assert_eq!(other_net.allowed(), 2);
    }

    #[test]
    fn remembered_limits_apply_to_new_servers() {
        let s = ServerLimits::new(8);
        s.remember(ServerLimit {
            host: "lento.com".into(),
            network: "sistema".into(),
            max: 2,
        });
        assert_eq!(s.limiter("lento.com", "sistema").allowed(), 2);
        assert_eq!(s.limiter("rapido.com", "sistema").allowed(), 8);
        assert_eq!(
            s.learned(),
            vec![ServerLimit {
                host: "lento.com".into(),
                network: "sistema".into(),
                max: 2
            }]
        );
    }
}
