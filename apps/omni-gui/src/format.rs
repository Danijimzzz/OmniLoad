//! Textos para la interfaz: tamaños, velocidades y tiempos.

use std::time::Duration;

/// Bytes en unidades binarias con un decimal: "1,5 GiB".
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit]).replace('.', ",")
}

/// Velocidad: "12,3 MiB/s".
pub fn speed(bytes_per_sec: f64) -> String {
    format!("{}/s", bytes(bytes_per_sec.max(0.0) as u64))
}

/// Tiempo restante: "45 s", "3 min 20 s", "2 h 5 min".
pub fn duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s} s"),
        60..3600 => format!("{} min {} s", s / 60, s % 60),
        _ => format!("{} h {} min", s / 3600, (s % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1536), "1,5 KiB");
        assert_eq!(bytes(3 * 1024 * 1024 * 1024), "3,0 GiB");
    }

    #[test]
    fn times() {
        assert_eq!(duration(Duration::from_secs(45)), "45 s");
        assert_eq!(duration(Duration::from_secs(200)), "3 min 20 s");
        assert_eq!(duration(Duration::from_secs(7500)), "2 h 5 min");
    }
}
