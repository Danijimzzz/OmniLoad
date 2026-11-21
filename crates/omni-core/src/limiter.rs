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

    /// El servidor rechazó una conexión (que ya soltó su hueco). Las que
    /// siguen ocupando hueco son las que el servidor aceptó o aún no ha
    /// contestado, así que el límite baja a ese número. Devuelve `true` si el
    /// rechazo se debía a un exceso de conexiones y `false` si el límite ya
    /// era 1 y no quedaba ninguna otra (el servidor rechaza incluso una
    /// sola: es un error de verdad).
    pub fn reject(&mut self, now: Instant) -> bool {
        let burst = self
            .last_reject
            .is_some_and(|t| now.duration_since(t) < REJECT_WINDOW);
        self.last_reject = Some(now);
        self.last_change = now;
        if burst {
            // Mismo golpe de rechazos: bajar hasta las que siguen dentro, sin
            // restar una más por cada rechazo.
            self.allowed = self.active.clamp(1, self.allowed);
            return true;
        }
        if self.allowed <= 1 && self.active == 0 {
            return false;
        }
        self.allowed = self.active.clamp(1, self.allowed.saturating_sub(1).max(1));
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

    pub fn reject(&self) -> bool {
        self.lock().reject(Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 8 conexiones contra un servidor que acepta `accepts`: las de más
    /// reciben el rechazo una tras otra y sueltan su hueco.
    fn burst(s: &mut LimitState, now: Instant, conns: usize, accepts: usize) {
        for _ in 0..conns {
            assert!(s.try_acquire(now));
        }
        for _ in accepts..conns {
            s.release();
            assert!(s.reject(now));
        }
    }

    #[test]
    fn settles_on_what_the_server_accepts() {
        let t0 = Instant::now();
        let mut s = LimitState::new(8, t0, GROW_AFTER);
        burst(&mut s, t0, 8, 5);
        assert_eq!(s.allowed(), 5);
    }

    #[test]
    fn grows_back_after_a_quiet_minute() {
        let t0 = Instant::now();
        let mut s = LimitState::new(8, t0, GROW_AFTER);
        burst(&mut s, t0, 8, 2);
        assert_eq!(s.allowed(), 2);
        assert!(!s.try_acquire(t0));
        let t1 = t0 + GROW_AFTER;
        assert!(s.try_acquire(t1));
        assert_eq!(s.allowed(), 3);
        assert!(!s.try_acquire(t1));
    }

    #[test]
    fn a_later_rejection_lowers_by_at_least_one() {
        let t0 = Instant::now();
        let mut s = LimitState::new(8, t0, GROW_AFTER);
        for _ in 0..8 {
            assert!(s.try_acquire(t0));
        }
        s.release();
        assert!(s.reject(t0));
        assert_eq!(s.allowed(), 7);
    }

    #[test]
    fn single_connection_rejected_is_a_real_error() {
        let t0 = Instant::now();
        let mut s = LimitState::new(4, t0, GROW_AFTER);
        burst(&mut s, t0, 4, 0);
        assert_eq!(s.allowed(), 1);
        let t1 = t0 + Duration::from_secs(5);
        assert!(s.try_acquire(t1));
        s.release();
        assert!(!s.reject(t1));
        assert_eq!(s.allowed(), 1);
    }
}
