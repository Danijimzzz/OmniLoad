//! Velocidad y tiempo restante que se leen bien: cambian una vez por segundo
//! y no saltan de un valor a otro.
//!
//! El motor ya suaviza la velocidad un poco, pero la ventana se redibuja
//! cuatro veces por segundo y el tiempo restante (bytes que faltan entre
//! velocidad) amplifica cualquier tirón. Aquí cada fila (archivo, paquete o
//! el total) tiene su propia velocidad mostrada:
//!
//! - Se actualiza como mucho una vez por segundo, con una media exponencial
//!   más lenta que la del motor.
//! - El tiempo restante se calcula con esa velocidad en cada actualización y,
//!   entre una y otra, solo cuenta hacia atrás un segundo por segundo.
//! - Se redondea: al segundo hasta 1 min, a 5 s hasta 1 h y al minuto a
//!   partir de ahí. Así `3 min 20 s` no baila a `3 min 14 s` y `3 min 27 s`.
//! - Si la velocidad cae a cero (pausa, error) se vacía al momento, sin
//!   esperar.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Cada cuánto cambia lo que se muestra.
const PERIOD: Duration = Duration::from_secs(1);
/// Peso de la muestra nueva en la media (por actualización).
const ALPHA: f64 = 0.2;

#[derive(Debug)]
struct Entry {
    updated: Instant,
    seen: Instant,
    /// Media de la velocidad.
    avg: f64,
    /// Tiempo restante en el momento de `updated`.
    eta: Option<Duration>,
}

/// Lo que se enseña de una fila.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shown {
    pub rate: f64,
    pub eta: Option<Duration>,
}

#[derive(Debug, Default)]
pub struct Smoother {
    rows: HashMap<String, Entry>,
}

impl Smoother {
    /// `rate`: velocidad de ahora (B/s); `remaining`: bytes que faltan, si
    /// se sabe.
    pub fn shown(&mut self, key: &str, now: Instant, rate: f64, remaining: Option<u64>) -> Shown {
        if rate <= 0.0 {
            self.rows.remove(key);
            return Shown {
                rate: 0.0,
                eta: None,
            };
        }
        let e = self.rows.entry(key.to_owned()).or_insert(Entry {
            updated: now - PERIOD,
            seen: now,
            avg: 0.0,
            eta: None,
        });
        e.seen = now;
        if now.duration_since(e.updated) >= PERIOD {
            e.avg = if e.avg == 0.0 {
                rate
            } else {
                (1.0 - ALPHA) * e.avg + ALPHA * rate
            };
            e.eta = remaining
                .filter(|&r| r > 0)
                .map(|r| Duration::from_secs_f64(r as f64 / e.avg));
            e.updated = now;
        }
        let eta = match (e.eta, remaining) {
            (_, Some(0)) => None,
            (Some(at), _) => Some(round_eta(at.saturating_sub(now.duration_since(e.updated)))),
            (None, _) => None,
        };
        Shown { rate: e.avg, eta }
    }

    /// Olvida las filas que ya no se pintan (para que no crezca sin fin).
    pub fn forget_stale(&mut self, now: Instant) {
        self.rows
            .retain(|_, e| now.duration_since(e.seen) < Duration::from_secs(5));
    }
}

/// Redondea el tiempo restante para que no baile.
pub fn round_eta(d: Duration) -> Duration {
    let s = d.as_secs_f64().round() as u64;
    let s = match s {
        0..60 => s,
        60..3600 => (s + 2) / 5 * 5,
        _ => (s + 30) / 60 * 60,
    };
    Duration::from_secs(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn eta_rounding() {
        assert_eq!(round_eta(secs(45)), secs(45));
        assert_eq!(round_eta(secs(200)), secs(200));
        assert_eq!(round_eta(secs(203)), secs(205));
        assert_eq!(round_eta(secs(197)), secs(195));
        assert_eq!(
            round_eta(secs(3 * 3600 + 29 * 60 + 40)),
            secs(3 * 3600 + 30 * 60)
        );
    }

    #[test]
    fn changes_once_per_second() {
        let mut s = Smoother::default();
        let t0 = Instant::now();
        let a = s.shown("f1", t0, 1_000_000.0, Some(100_000_000));
        // Un tirón dentro del mismo segundo no cambia la velocidad mostrada.
        let b = s.shown(
            "f1",
            t0 + Duration::from_millis(250),
            5_000_000.0,
            Some(99_000_000),
        );
        assert_eq!(a.rate, b.rate);
        assert_eq!(a.rate, 1_000_000.0);
        // El tiempo restante solo cuenta hacia atrás: ~100 s → ~99 s.
        assert_eq!(a.eta, Some(secs(100)));
        let c = s.shown(
            "f1",
            t0 + Duration::from_secs(1),
            1_000_000.0,
            Some(99_000_000),
        );
        assert_eq!(c.eta, Some(secs(100))); // se resincroniza: 99 s / 1 MB/s = 99 → 100 por redondeo a 5
    }

    #[test]
    fn spike_barely_moves_the_average_or_the_eta() {
        let mut s = Smoother::default();
        let t0 = Instant::now();
        let mut remaining = 600_000_000u64;
        let mut last = None;
        for i in 0..20u64 {
            // Velocidad de 2 MB/s con un tirón a 6 MB/s en el segundo 10.
            let raw = if i == 10 { 6_000_000.0 } else { 2_000_000.0 };
            let sh = s.shown("p", t0 + secs(i), raw, Some(remaining));
            remaining = remaining.saturating_sub(2_000_000);
            if i == 9 {
                last = Some(sh);
            }
            if i == 10 {
                let before = last.unwrap();
                let jump = sh.eta.unwrap().as_secs_f64() / before.eta.unwrap().as_secs_f64();
                assert!(jump > 0.7, "el tiempo restante saltó demasiado: {jump}");
            }
        }
    }

    #[test]
    fn zero_rate_clears_immediately() {
        let mut s = Smoother::default();
        let t0 = Instant::now();
        s.shown("f", t0, 1_000.0, Some(10_000));
        let sh = s.shown("f", t0 + Duration::from_millis(100), 0.0, Some(10_000));
        assert_eq!(
            sh,
            Shown {
                rate: 0.0,
                eta: None
            }
        );
    }

    #[test]
    fn finished_has_no_eta_and_stale_rows_are_forgotten() {
        let mut s = Smoother::default();
        let t0 = Instant::now();
        assert_eq!(s.shown("f", t0, 1_000.0, Some(0)).eta, None);
        s.forget_stale(t0 + secs(10));
        assert!(s.rows.is_empty());
    }
}
