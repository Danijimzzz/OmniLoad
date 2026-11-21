use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::NetworkId;

/// Progreso de una descarga, compartido con quien la muestra (la CLI hoy, la
/// interfaz Slint en la fase 4). Se lee sin bloquear el motor.
#[derive(Debug, Default)]
pub struct Progress {
    total: AtomicU64,
    total_known: AtomicBool,
    downloaded: AtomicU64,
    resumed_from: AtomicU64,
    per_network: Mutex<BTreeMap<NetworkId, NetworkStats>>,
}

/// Bytes y conexiones de una red.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetworkStats {
    pub bytes: u64,
    /// Conexiones vivas, incluidas las que esperan para reintentar.
    pub connections: usize,
    /// Conexiones recibiendo datos ahora mismo.
    pub receiving: usize,
    /// Reintentos por errores (cortes, 503, 429…) desde el inicio.
    pub retries: u64,
}

impl Progress {
    /// Tamaño total, si se conoce.
    pub fn total(&self) -> Option<u64> {
        self.total_known
            .load(Ordering::Relaxed)
            .then(|| self.total.load(Ordering::Relaxed))
    }

    /// Bytes escritos en disco (sin contar duplicados de bloques en
    /// competición).
    pub fn downloaded(&self) -> u64 {
        self.downloaded.load(Ordering::Relaxed)
    }

    /// Bytes que ya había al reanudar.
    pub fn resumed_from(&self) -> u64 {
        self.resumed_from.load(Ordering::Relaxed)
    }

    /// Bytes y conexiones por red.
    pub fn per_network(&self) -> BTreeMap<NetworkId, NetworkStats> {
        self.per_network
            .lock()
            .expect("progreso envenenado")
            .clone()
    }

    pub(crate) fn set_total(&self, total: Option<u64>) {
        self.total.store(total.unwrap_or(0), Ordering::Relaxed);
        self.total_known.store(total.is_some(), Ordering::Relaxed);
    }

    pub(crate) fn reset(&self, downloaded: u64) {
        self.downloaded.store(downloaded, Ordering::Relaxed);
        self.resumed_from.store(downloaded, Ordering::Relaxed);
    }

    pub(crate) fn add(&self, network: NetworkId, bytes: u64) {
        self.downloaded.fetch_add(bytes, Ordering::Relaxed);
        self.per_network
            .lock()
            .expect("progreso envenenado")
            .entry(network)
            .or_default()
            .bytes += bytes;
    }

    pub(crate) fn connection_opened(&self, network: NetworkId) {
        self.per_network
            .lock()
            .expect("progreso envenenado")
            .entry(network)
            .or_default()
            .connections += 1;
    }

    fn with_stats(&self, network: NetworkId, f: impl FnOnce(&mut NetworkStats)) {
        f(self
            .per_network
            .lock()
            .expect("progreso envenenado")
            .entry(network)
            .or_default());
    }

    pub(crate) fn receiving_started(&self, network: NetworkId) {
        self.with_stats(network, |s| s.receiving += 1);
    }

    pub(crate) fn receiving_stopped(&self, network: NetworkId) {
        self.with_stats(network, |s| s.receiving = s.receiving.saturating_sub(1));
    }

    pub(crate) fn retried(&self, network: NetworkId) {
        self.with_stats(network, |s| s.retries += 1);
    }

    pub(crate) fn connection_closed(&self, network: NetworkId) {
        if let Some(s) = self
            .per_network
            .lock()
            .expect("progreso envenenado")
            .get_mut(&network)
        {
            s.connections = s.connections.saturating_sub(1);
        }
    }
}
