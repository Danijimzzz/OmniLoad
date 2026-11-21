//! Límite adaptativo de conexiones por red.
//!
//! Muchos servidores limitan las conexiones simultáneas por IP y responden
//! 429 o 503 a las que sobran. Insistir con todas solo acumula esperas cada
//! vez más largas. En su lugar, al primer rechazo se baja el límite al número
//! de conexiones que sí están recibiendo datos, y cada `grow_after` sin
//! rechazos se prueba con una más, hasta el máximo pedido.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Cada cuánto, sin rechazos, se prueba con una conexión más.
pub const GROW_AFTER: Duration = Duration::from_secs(60);

/// Rechazos que llegan juntos cuentan como uno: cuando el servidor limita,
/// todas las conexiones de más reciben el 429 casi a la vez.
const REJECT_WINDOW: Duration = Duration::from_secs(2);

/// Estado del límite, sin E/S ni tiempo real (se prueba con `Instant`
/// inventados).
#[derive(Debug)]
pub struct LimitState {
    max: usize,
    allowed: usize,
    active: usize,
    last_change: Instant,
    last_reject: Option<Instant>,
    grow_after: Duration,
}

impl LimitState {
    pub fn new(max: usize, now: Instant, grow_after: Duration) -> Self {
        let max = max.max(1);
        Self {
            max,
            allowed: max,
            active: 0,
            last_change: now,
            last_reject: None,
            grow_after,
        }
    }

    pub fn allowed(&self) -> usize {
        self.allowed
    }

    /// Ocupa un hueco si hay. Antes, sube el límite si toca.
    pub fn try_acquire(&mut self, now: Instant) -> bool {
        self.maybe_grow(now);
        if self.active < self.allowed {
            self.active += 1;
            true
        } else {
            false
        }
    }

    pub fn release(&mut self) {
        self.active = self.active.saturating_sub(1);
    }

    /// El servidor rechazó una conexión mientras `receiving` recibían datos.
    /// Devuelve `true` si el límite bajó (el rechazo se debía a que había
    /// demasiadas conexiones) y `false` si ya estaba en 1 (el servidor
    /// rechaza incluso una sola: es un error de verdad).
    pub fn reject(&mut self, receiving: usize, now: Instant) -> bool {
        let burst = self
            .last_reject
            .is_some_and(|t| now.duration_since(t) < REJECT_WINDOW);
        if burst {
            // Ya se bajó por este mismo golpe de rechazos.
            self.last_change = now;
            return true;
        }
        self.last_reject = Some(now);
        if self.allowed <= 1 {
            self.last_change = now;
            return false;
        }
        let target = receiving.clamp(1, self.allowed - 1);
        self.allowed = target;
        self.last_change = now;
        true
    }

    fn maybe_grow(&mut self, now: Instant) {
        if self.allowed < self.max && now.duration_since(self.last_change) >= self.grow_after {
            self.allowed += 1;
            self.last_change = now;
        }
    }
}

/// Límite compartido por las conexiones de una red.
pub struct Limiter {
    state: Mutex<LimitState>,
    notify: Notify,
}

impl Limiter {
    pub fn new(max: usize, grow_after: Duration) -> Self {
        Self {
            state: Mutex::new(LimitState::new(max, Instant::now(), grow_after)),
            notify: Notify::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LimitState> {
        self.state.lock().expect("límite envenenado")
    }

    pub fn allowed(&self) -> usize {
        self.lock().allowed()
    }

    /// Espera un hueco. `false` si se canceló la descarga.
    pub async fn acquire(&self, abort: &CancellationToken) -> bool {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.lock().try_acquire(Instant::now()) {
                return true;
            }
            tokio::select! {
                _ = abort.cancelled() => return false,
                _ = &mut notified => {}
                // Revisar de vez en cuando por si toca subir el límite.
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    }

    pub fn release(&self) {
        self.lock().release();
        self.notify.notify_waiters();
    }

    pub fn reject(&self, receiving: usize) -> bool {
        self.lock().reject(receiving, Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowers_to_receiving_and_grows_back() {
        let t0 = Instant::now();
        let mut s = LimitState::new(8, t0, GROW_AFTER);
        for _ in 0..8 {
            assert!(s.try_acquire(t0));
        }
        assert!(!s.try_acquire(t0));
        // 6 conexiones rechazadas mientras 2 reciben datos.
        assert!(s.reject(2, t0));
        assert_eq!(s.allowed(), 2);
        for _ in 0..8 {
            s.release();
        }
        assert!(s.try_acquire(t0));
        assert!(s.try_acquire(t0));
        assert!(!s.try_acquire(t0));
        // A los 60 s sin rechazos se prueba con una más.
        let t1 = t0 + GROW_AFTER;
        assert!(s.try_acquire(t1));
        assert_eq!(s.allowed(), 3);
        assert!(!s.try_acquire(t1));
    }

    #[test]
    fn never_below_one() {
        let t0 = Instant::now();
        let mut s = LimitState::new(4, t0, GROW_AFTER);
        assert!(s.reject(0, t0));
        assert_eq!(s.allowed(), 1);
        // Con una sola conexión, un rechazo ya no baja nada: es un fallo real.
        let t1 = t0 + Duration::from_secs(5);
        assert!(!s.reject(0, t1));
        assert_eq!(s.allowed(), 1);
    }

    #[test]
    fn simultaneous_rejections_count_once() {
        let t0 = Instant::now();
        let mut s = LimitState::new(8, t0, GROW_AFTER);
        for _ in 0..6 {
            assert!(s.reject(2, t0));
        }
        assert_eq!(s.allowed(), 2);
    }

    #[test]
    fn each_rejection_lowers_at_least_one() {
        let t0 = Instant::now();
        let mut s = LimitState::new(8, t0, GROW_AFTER);
        assert!(s.reject(8, t0));
        assert_eq!(s.allowed(), 7);
    }
}
