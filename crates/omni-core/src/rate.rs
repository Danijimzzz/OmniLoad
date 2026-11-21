//! Límite de velocidad.
//!
//! Un [`RateLimit`] es un cubo de fichas compartido: cada conexión pide
//! permiso por los bytes que acaba de recibir y, si se ha pasado, espera lo
//! justo para que la media no supere el límite. Se puede compartir entre
//! varias descargas (límite global) y combinar con otro propio de cada
//! descarga; el límite se cambia en caliente con [`RateLimit::set`].
//!
//! Al dejar de leer del socket, TCP frena al servidor por sí solo, así que no
//! se descarga nada de más.

use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Espera máxima de una vez: así un cambio de límite (o quitarlo) se nota
/// enseguida.
const MAX_SLEEP: Duration = Duration::from_millis(100);

/// Límite de velocidad compartido, en bytes por segundo.
#[derive(Debug)]
pub struct RateLimit {
    state: Mutex<Bucket>,
}

#[derive(Debug)]
struct Bucket {
    /// Bytes por segundo; `None` = sin límite.
    rate: Option<u64>,
    /// Fichas disponibles (negativo = deuda que hay que esperar).
    tokens: f64,
    last: Instant,
}

impl Bucket {
    /// Ráfaga máxima acumulada: un cuarto de segundo (mínimo 16 KiB), para
    /// que tras un rato parado no se dispare la velocidad.
    fn burst(rate: u64) -> f64 {
        (rate as f64 / 4.0).max(16.0 * 1024.0)
    }

    fn refill(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        if let Some(rate) = self.rate {
            self.tokens = (self.tokens + dt * rate as f64).min(Self::burst(rate));
        }
    }
}

impl Default for RateLimit {
    fn default() -> Self {
        Self::new(None)
    }
}

impl RateLimit {
    /// `bytes_per_sec`: `None` o `Some(0)` = sin límite.
    pub fn new(bytes_per_sec: Option<u64>) -> Self {
        Self {
            state: Mutex::new(Bucket {
                rate: bytes_per_sec.filter(|&r| r > 0),
                tokens: 0.0,
                last: Instant::now(),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Bucket> {
        self.state.lock().expect("límite de velocidad envenenado")
    }

    /// Límite actual en bytes por segundo (`None` = sin límite).
    pub fn get(&self) -> Option<u64> {
        self.lock().rate
    }

    /// Cambia el límite; vale para las descargas que ya están en marcha.
    pub fn set(&self, bytes_per_sec: Option<u64>) {
        let mut b = self.lock();
        b.refill(Instant::now());
        b.rate = bytes_per_sec.filter(|&r| r > 0);
        match b.rate {
            None => b.tokens = 0.0,
            Some(r) => b.tokens = b.tokens.clamp(-(r as f64), Bucket::burst(r)),
        }
    }

    /// Cuenta `bytes` recibidos y espera lo necesario para no pasar del
    /// límite. Devuelve `false` si se canceló mientras esperaba.
    pub async fn consume(&self, bytes: u64, cancel: &CancellationToken) -> bool {
        {
            let mut b = self.lock();
            if b.rate.is_none() {
                return true;
            }
            b.refill(Instant::now());
            b.tokens -= bytes as f64;
        }
        loop {
            let wait = {
                let mut b = self.lock();
                b.refill(Instant::now());
                match b.rate {
                    None => return true,
                    Some(_) if b.tokens >= 0.0 => return true,
                    Some(rate) => Duration::from_secs_f64(-b.tokens / rate as f64),
                }
            };
            tokio::select! {
                _ = cancel.cancelled() => return false,
                _ = tokio::time::sleep(wait.min(MAX_SLEEP)) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn unlimited_never_waits() {
        let r = RateLimit::new(None);
        let t0 = Instant::now();
        assert!(r.consume(1 << 30, &CancellationToken::new()).await);
        assert_eq!(Instant::now(), t0);
    }

    #[tokio::test(start_paused = true)]
    async fn keeps_the_average_under_the_limit() {
        let r = RateLimit::new(Some(100_000));
        let cancel = CancellationToken::new();
        let t0 = Instant::now();
        for _ in 0..50 {
            assert!(r.consume(10_000, &cancel).await);
        }
        // 500 KB a 100 KB/s: unos 5 s (menos la ráfaga inicial).
        let took = Instant::now() - t0;
        assert!(took >= Duration::from_millis(4_700), "{took:?}");
        assert!(took <= Duration::from_millis(5_200), "{took:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn removing_the_limit_wakes_waiters() {
        let r = std::sync::Arc::new(RateLimit::new(Some(1_000)));
        let cancel = CancellationToken::new();
        let r2 = r.clone();
        let c2 = cancel.clone();
        let t0 = Instant::now();
        let waiter = tokio::spawn(async move { r2.consume(1_000_000, &c2).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        r.set(None);
        assert!(waiter.await.unwrap());
        assert!(Instant::now() - t0 < Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_interrupts_the_wait() {
        let r = RateLimit::new(Some(1));
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(!r.consume(1_000_000, &cancel).await);
    }
}
