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
//!
//! Un mismo límite lo comparten todas las descargas al mismo servidor por la
//! misma red (ver [`crate::ServerLimits`]): el servidor cuenta las conexiones
//! por IP, no por archivo. Además del máximo que acepta el servidor hay un
//! tope que elige el usuario ("conexiones por servidor").
//!
//! Reparto justo: cada descarga que comparte el límite es un "dueño". Si una
//! descarga espera hueco y tiene menos de su parte (límite / descargas), las
//! que tienen más que su parte no cogen huecos nuevos y sueltan el suyo al
//! terminar el bloque en curso. Así una descarga lenta (por ejemplo, con
//! límite de velocidad) no deja sin conexiones a las demás.

use std::collections::HashMap;
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
    /// Tope del usuario.
    cap: usize,
    /// ¿El servidor ha rechazado conexiones alguna vez? Entonces `proven` es
    /// su máximo de verdad.
    rejected: bool,
    /// Descargas que comparten el límite.
    owners: HashMap<u64, Owner>,
}

#[derive(Debug, Default, Clone, Copy)]
struct Owner {
    active: usize,
    /// Conexiones de esta descarga esperando hueco.
    waiting: usize,
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
            cap: max.max(1),
            rejected: false,
            owners: HashMap::new(),
        }
    }

    pub fn register(&mut self, owner: u64) {
        self.owners.entry(owner).or_default();
    }

    pub fn unregister(&mut self, owner: u64) {
        self.owners.remove(&owner);
    }

    pub fn set_waiting(&mut self, owner: u64, waiting: bool) {
        let o = self.owners.entry(owner).or_default();
        if waiting {
            o.waiting += 1;
        } else {
            o.waiting = o.waiting.saturating_sub(1);
        }
    }

    /// Parte justa de cada descarga.
    fn share(&self) -> usize {
        self.allowed.div_ceil(self.owners.len().max(1)).max(1)
    }

    fn mine(&self, owner: u64) -> usize {
        self.owners.get(&owner).map_or(0, |o| o.active)
    }

    /// ¿Otra descarga espera hueco con menos de su parte?
    fn others_starving(&self, owner: u64) -> bool {
        let share = self.share();
        self.owners
            .iter()
            .any(|(&o, s)| o != owner && s.waiting > 0 && s.active < share)
    }

    /// ¿Debería esta descarga soltar una conexión para dejar sitio a otra?
    pub fn should_yield(&self, owner: u64) -> bool {
        self.mine(owner) > self.share() && self.others_starving(owner)
    }

    /// Como [`LimitState::new`], pero sabiendo ya cuántas conexiones acepta
    /// el servidor (aprendido en otra sesión).
    pub fn with_known(max: usize, known: usize, now: Instant, regrow_after: Duration) -> Self {
        let mut s = Self::new(max, now, regrow_after);
        s.proven = known.max(1);
        s.rejected = true;
        s.allowed = s.cap.min(s.proven);
        s
    }

    pub fn allowed(&self) -> usize {
        self.allowed
    }

    pub fn active(&self) -> usize {
        self.active
    }

    /// Máximo que acepta el servidor, si se sabe.
    pub fn learned(&self) -> Option<usize> {
        self.rejected.then_some(self.proven.max(1))
    }

    /// Cambia el tope del usuario. Si el servidor nunca ha rechazado nada,
    /// el límite pasa a ser el tope nuevo; si no, nunca supera lo comprobado.
    pub fn set_cap(&mut self, cap: usize) {
        self.cap = cap.max(1);
        self.allowed = if self.rejected {
            self.allowed.min(self.cap)
        } else {
            self.cap
        };
    }

    /// Ocupa un hueco si hay. Antes, recupera una conexión si toca.
    pub fn try_acquire(&mut self, owner: u64, now: Instant) -> bool {
        self.maybe_regrow(now);
        if self.active >= self.allowed {
            return false;
        }
        if self.mine(owner) >= self.share() && self.others_starving(owner) {
            return false;
        }
        self.active += 1;
        self.owners.entry(owner).or_default().active += 1;
        true
    }

    pub fn release(&mut self, owner: u64) {
        self.active = self.active.saturating_sub(1);
        if let Some(o) = self.owners.get_mut(&owner) {
            o.active = o.active.saturating_sub(1);
        }
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
        self.rejected = true;
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
        if self.allowed < self.proven.min(self.cap)
            && now.duration_since(self.last_change) >= self.regrow_after
        {
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

    pub fn with_known(max: usize, known: usize, regrow_after: Duration) -> Self {
        Self {
            state: Mutex::new(LimitState::with_known(
                max,
                known,
                Instant::now(),
                regrow_after,
            )),
            notify: Notify::new(),
        }
    }

    pub fn active(&self) -> usize {
        self.lock().active()
    }

    pub fn learned(&self) -> Option<usize> {
        self.lock().learned()
    }

    pub fn set_cap(&self, cap: usize) {
        self.lock().set_cap(cap);
        self.notify.notify_waiters();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LimitState> {
        self.state.lock().expect("límite envenenado")
    }

    pub fn allowed(&self) -> usize {
        self.lock().allowed()
    }

    pub fn register(&self, owner: u64) {
        self.lock().register(owner);
    }

    pub fn unregister(&self, owner: u64) {
        self.lock().unregister(owner);
        self.notify.notify_waiters();
    }

    pub fn should_yield(&self, owner: u64) -> bool {
        self.lock().should_yield(owner)
    }

    /// Espera un hueco. `false` si se canceló la descarga.
    pub async fn acquire(&self, owner: u64, abort: &CancellationToken) -> bool {
        let mut waiting = false;
        let got = loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut st = self.lock();
                if st.try_acquire(owner, Instant::now()) {
                    if waiting {
                        st.set_waiting(owner, false);
                    }
                    return true;
                }
                if !waiting {
                    st.set_waiting(owner, true);
                    waiting = true;
                }
            }
            tokio::select! {
                _ = abort.cancelled() => break false,
                _ = &mut notified => {}
                // Revisar de vez en cuando por si toca recuperar una conexión.
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        };
        if waiting {
            self.lock().set_waiting(owner, false);
        }
        got
    }

    pub fn release(&self, owner: u64) {
        self.lock().release(owner);
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
            assert!(s.try_acquire(0, now));
        }
        for _ in 0..accepts {
            s.receiving_started();
        }
        for _ in accepts..conns {
            s.release(0);
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
            assert!(!s.try_acquire(0, t0 + later));
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
        s.release(0);
        assert!(s.try_acquire(0, t1));
        s.release(0);
        assert!(s.reject(t1));
        assert_eq!(s.allowed(), 2);
        // A los pocos segundos vuelve a 3, que ya se sabe que funciona.
        let t2 = t1 + REGROW_AFTER;
        assert!(s.try_acquire(0, t2));
        assert_eq!(s.allowed(), 3);
    }

    #[test]
    fn a_later_rejection_lowers_by_at_least_one() {
        let t0 = Instant::now();
        let mut s = LimitState::new(8, t0, REGROW_AFTER);
        for _ in 0..8 {
            assert!(s.try_acquire(0, t0));
        }
        s.release(0);
        assert!(s.reject(t0));
        assert_eq!(s.allowed(), 7);
    }

    #[test]
    fn user_cap_is_respected_and_can_grow_until_the_server_says_no() {
        let t0 = Instant::now();
        let mut s = LimitState::new(8, t0, REGROW_AFTER);
        s.set_cap(2);
        assert!(s.try_acquire(0, t0));
        assert!(s.try_acquire(0, t0));
        assert!(!s.try_acquire(0, t0));
        // Sin rechazos, subir el tope sube el límite.
        s.set_cap(6);
        assert_eq!(s.allowed(), 6);
        assert_eq!(s.learned(), None);
    }

    #[test]
    fn known_limit_is_not_exceeded_by_a_bigger_cap() {
        let t0 = Instant::now();
        let mut s = LimitState::with_known(8, 3, t0, REGROW_AFTER);
        assert_eq!(s.allowed(), 3);
        assert_eq!(s.learned(), Some(3));
        s.set_cap(10);
        assert_eq!(s.allowed(), 3);
        s.set_cap(2);
        assert_eq!(s.allowed(), 2);
        // Ni recuperando conexiones pasa del tope del usuario.
        let later = t0 + Duration::from_secs(60);
        assert!(s.try_acquire(0, later));
        assert!(s.try_acquire(0, later));
        assert!(!s.try_acquire(0, later));
        assert_eq!(s.allowed(), 2);
    }

    #[test]
    fn a_new_download_gets_its_fair_share() {
        let t0 = Instant::now();
        let mut s = LimitState::new(4, t0, REGROW_AFTER);
        s.register(1);
        for _ in 0..4 {
            assert!(s.try_acquire(1, t0));
        }
        // Llega otra descarga y espera: la primera debe soltar hasta su parte.
        s.register(2);
        s.set_waiting(2, true);
        assert!(!s.try_acquire(2, t0));
        assert!(s.should_yield(1));
        s.release(1);
        assert!(s.should_yield(1));
        assert!(!s.try_acquire(1, t0), "no debe recuperar el hueco");
        assert!(s.try_acquire(2, t0));
        s.release(1);
        assert!(!s.should_yield(1), "con 2 de 4 ya tiene su parte");
        assert!(s.try_acquire(2, t0));
        s.set_waiting(2, false);
        // Si nadie espera, se puede pasar de la parte.
        s.release(2);
        s.release(2);
        s.unregister(2);
        assert!(s.try_acquire(1, t0));
    }

    #[test]
    fn single_connection_rejected_is_a_real_error() {
        let t0 = Instant::now();
        let mut s = LimitState::new(4, t0, REGROW_AFTER);
        burst(&mut s, t0, 4, 0);
        assert_eq!(s.allowed(), 1);
        let t1 = t0 + Duration::from_secs(5);
        assert!(s.try_acquire(0, t1));
        s.release(0);
        assert!(!s.reject(t1));
        assert_eq!(s.allowed(), 1);
    }
}
