use std::time::Duration;

use reqwest::header::{HeaderMap, RETRY_AFTER};

/// Espera exponencial: base, 2·base, 4·base… hasta `max`.
#[derive(Clone, Copy, Debug)]
pub struct Backoff {
    pub base: Duration,
    pub max: Duration,
}

impl Backoff {
    pub fn delay(&self, attempt: u32) -> Duration {
        let factor = 1u32.checked_shl(attempt.min(16)).unwrap_or(u32::MAX);
        self.base.saturating_mul(factor).min(self.max)
    }
}

/// Lee `Retry-After` en segundos (la forma con fecha se ignora).
pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let secs: u64 = headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_and_caps() {
        let b = Backoff {
            base: Duration::from_secs(1),
            max: Duration::from_secs(60),
        };
        assert_eq!(b.delay(0), Duration::from_secs(1));
        assert_eq!(b.delay(3), Duration::from_secs(8));
        assert_eq!(b.delay(10), Duration::from_secs(60));
        assert_eq!(b.delay(100), Duration::from_secs(60));
    }
}
