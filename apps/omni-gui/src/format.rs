//! Textos para la interfaz: tamaños, velocidades y tiempos.

use std::sync::atomic::{AtomicI32, Ordering};
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

static SPEED_UNIT: AtomicI32 = AtomicI32::new(0);

/// Unidad de las velocidades: 0 MiB/s, 1 MB/s, 2 KiB/s, 3 Mbit/s.
pub fn set_speed_unit(unit: i32) {
    SPEED_UNIT.store(unit, Ordering::Relaxed);
}

/// Número con un decimal y coma: "12,3".
fn one_decimal(v: f64) -> String {
    format!("{v:.1}").replace('.', ",")
}

/// Velocidad en la unidad elegida: "12,3 MiB/s", "12,9 MB/s", "12 595 KiB/s"
/// o "103,2 Mbit/s".
pub fn speed(bytes_per_sec: f64) -> String {
    let v = bytes_per_sec.max(0.0);
    match SPEED_UNIT.load(Ordering::Relaxed) {
        1 => scaled(v, 1000.0, &["B/s", "KB/s", "MB/s", "GB/s"]),
        2 => format!("{} KiB/s", (v / 1024.0).round() as u64),
        3 => scaled(v * 8.0, 1000.0, &["bit/s", "kbit/s", "Mbit/s", "Gbit/s"]),
        _ => format!("{}/s", bytes(v as u64)),
    }
}

/// Divide por `base` hasta que quepa y le pone la unidad que toca.
fn scaled(mut v: f64, base: f64, units: &[&str]) -> String {
    let mut unit = 0;
    while v >= base && unit < units.len() - 1 {
        v /= base;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", v as u64, units[0])
    } else {
        format!("{} {}", one_decimal(v), units[unit])
    }
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
    fn speed_units() {
        // Los tests comparten el estado global: una sola prueba lo cambia.
        let rate = 3.0 * 1024.0 * 1024.0;
        set_speed_unit(0);
        assert_eq!(speed(rate), "3,0 MiB/s");
        set_speed_unit(1);
        assert_eq!(speed(rate), "3,1 MB/s");
        set_speed_unit(2);
        assert_eq!(speed(rate), "3072 KiB/s");
        set_speed_unit(3);
        assert_eq!(speed(rate), "25,2 Mbit/s");
        set_speed_unit(0);
    }

    #[test]
    fn times() {
        assert_eq!(duration(Duration::from_secs(45)), "45 s");
        assert_eq!(duration(Duration::from_secs(200)), "3 min 20 s");
        assert_eq!(duration(Duration::from_secs(7500)), "2 h 5 min");
    }
}
