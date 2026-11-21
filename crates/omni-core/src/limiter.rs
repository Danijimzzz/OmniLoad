//! Límite adaptativo de conexiones por red.
//!
//! Muchos servidores limitan las conexiones simultáneas por IP y responden
//! 429 o 503 a las que sobran. Insistir con todas solo acumula esperas cada
//! vez más largas. En su lugar:
//!
//! 1. Al recibir rechazos, el límite baja a las conexiones que siguen dentro
//!    (las que el servidor aceptó o aún no ha contestado).
//! 2. Se recuerda cuántas conexiones llegaron a recibir datos a la vez: ese es
//!    el máximo que el servidor acepta de verdad.
//! 3. Si el límite cae por debajo de ese máximo (por ejemplo, porque una
//!    conexión nueva llegó antes de que el servidor liberase la anterior),
//!    vuelve a él a los pocos segundos. Por encima nunca se prueba: una vez
//!    conocido el máximo del servidor, se trabaja con ese número y ya está.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Espera sin rechazos antes de recuperar una conexión hasta el máximo
/// comprobado.
pub const REGROW_AFTER: Duration = Duration::from_secs(5);

/// Rechazos que llegan juntos cuentan como uno: cuando el servidor limita,
/// todas las conexiones de más reciben el 429 casi a la vez.
const REJECT_WINDOW: Duration = Duration::from_secs(2);

/// Estado del límite, sin E/S ni tiempo real (se prueba con `Instant`
/// inventados).
#[derive(Debug)]
pub struct LimitState {
    allowed: usize,
    /// Conexiones con hueco ocupado (pidiendo o recibiendo).
    active: usize,
    /// Conexiones recibiendo datos ahora mismo.
    receiving: usize,
    /// Máximo de conexiones que llegaron a recibir datos a la vez.
    proven: usize,
    last_change: Instant,
    last_reject: Option<Instant>,
    regrow_after: Duration,
}

impl LimitState {
    pub fn new(max: usize, now: Instant, regrow_after: Duration) -> Self {
        Self {
            allowed: max.max(1),
            active: 0,
            receiving: 0,
            proven: 0,
            last_change: now,
            last_reject: None,
            regrow_after,
        }
    }

    pub fn allowed(&self) -> usize {
        self.allowed
    }

    /// Ocupa un hueco si hay. Antes, recupera una conexión si toca.
    pub fn try_acquire(&mut self, now: Instant) -> bool {
        self.maybe_regrow(now);
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

    /// Una conexión empezó a recibir datos: el servidor la aceptó.
    pub fn receiving_started(&mut self) {
        self.receiving += 1;
        self.proven = self.proven.max(self.receiving);
    }

    pub fn receiving_stopped(&mut self) {
        self.receiving = self.receiving.saturating_sub(1);
    }

    /// El servidor rechazó una conexión (que ya soltó su hueco). El límite
    /// baja a las que siguen ocupando hueco. Devuelve `true` si el rechazo se
    /// debía a un exceso de conexiones y `false` si el límite ya era 1 y no
    /// quedaba ninguna otra (el servidor rechaza incluso una sola: es un error
    /// de verdad).
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

    /// Vuelve, de una en una, hasta el máximo comprobado. Nunca lo supera.
    fn maybe_regrow(&mut self, now: Instant) {
        if self.allowed < self.proven && now.duration_since(self.last_change) >= self.regrow_after {
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
    pub fn new(max: usize, regrow_after: Duration) -> Self {
        Self {
            state: Mutex::new(LimitState::new(max, Instant::now(), regrow_after)),
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
                // Revisar de vez en cuando por si toca recuperar una conexión.
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

    pub fn receiving_started(&self) {
        self.lock().receiving_started();
    }

    pub fn receiving_stopped(&self) {
        self.lock().receiving_stopped();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `conns` conexiones contra un servidor que acepta `accepts`: las
    /// aceptadas empiezan a recibir y las de más reciben el rechazo y sueltan
    /// su hueco.
    fn burst(s: &mut LimitState, now: Instant, conns: usize, accepts: usize) {
        for _ in 0..conns {
            assert!(s.try_acquire(now));
        }
        for _ in 0..accepts {
            s.receiving_started();
        }
        for _ in accepts..conns {
            s.release();
            assert!(s.reject(now));
        }
    }

    #[test]
    fn settles_on_what_the_server_accepts() {
        let t0 = Instant::now();
        let mut s = LimitState::new(8, t0, REGROW_AFTER);
        burst(&mut s, t0, 8, 5);
        assert_eq!(s.allowed(), 5);
    }

    #[test]
    fn never_probes_above_the_proven_maximum() {
        let t0 = Instant::now();
        let mut s = LimitState::new(8, t0, REGROW_AFTER);
        burst(&mut s, t0, 8, 3);
        assert_eq!(s.allowed(), 3);
        // Ni un minuto ni una hora después se intenta una cuarta.
        for later in [Duration::from_secs(60), Duration::from_secs(3600)] {
            assert!(!s.try_acquire(t0 + later));
            assert_eq!(s.allowed(), 3);
        }
    }

    #[test]
    fn recovers_quickly_up_to_the_proven_maximum() {
        let t0 = Instant::now();
        let mut s = LimitState::new(8, t0, REGROW_AFTER);
        burst(&mut s, t0, 8, 3);
        // Más tarde una conexión termina, otra llega antes de que el servidor
        // libere la anterior y recibe un 429: el límite cae a 2.
        let t1 = t0 + Duration::from_secs(30);
        s.receiving_stopped();
        s.release();
        assert!(s.try_acquire(t1));
        s.release();
        assert!(s.reject(t1));
        assert_eq!(s.allowed(), 2);
        // A los pocos segundos vuelve a 3, que ya se sabe que funciona.
        let t2 = t1 + REGROW_AFTER;
        assert!(s.try_acquire(t2));
        assert_eq!(s.allowed(), 3);
    }

    #[test]
    fn a_later_rejection_lowers_by_at_least_one() {
        let t0 = Instant::now();
        let mut s = LimitState::new(8, t0, REGROW_AFTER);
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
        let mut s = LimitState::new(4, t0, REGROW_AFTER);
        burst(&mut s, t0, 4, 0);
        assert_eq!(s.allowed(), 1);
        let t1 = t0 + Duration::from_secs(5);
        assert!(s.try_acquire(t1));
        s.release();
        assert!(!s.reject(t1));
        assert_eq!(s.allowed(), 1);
    }
}
