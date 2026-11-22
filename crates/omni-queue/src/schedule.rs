//! Horario de descargas: los días y las horas en que se permite descargar.
//!
//! Fuera del horario la cola se comporta como en una pausa: lo que corre se
//! pausa guardando el progreso y no arranca nada; al empezar el siguiente
//! tramo todo sigue solo. Es independiente del botón «Pausar» del usuario.

use serde::{Deserialize, Serialize};

/// Minutos que tiene un día.
pub const DAY_MINUTES: u16 = 24 * 60;

/// Un horario semanal de un solo tramo (la misma hora para los días
/// elegidos).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Schedule {
    pub enabled: bool,
    /// Días en que **empieza** el tramo: bit 0 lunes … bit 6 domingo.
    pub days: u8,
    /// Hora de inicio, en minutos desde las 00:00.
    pub from_min: u16,
    /// Hora de fin, en minutos desde las 00:00. Si es menor o igual que el
    /// inicio, el tramo acaba al día siguiente (por ejemplo, de 23:00 a
    /// 07:00); si es igual, dura 24 horas.
    pub to_min: u16,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            enabled: false,
            days: ALL_DAYS,
            from_min: 0,
            to_min: 7 * 60,
        }
    }
}

/// Todos los días de la semana.
pub const ALL_DAYS: u8 = 0b0111_1111;

impl Schedule {
    /// ¿Se puede descargar ahora? `weekday`: 0 lunes … 6 domingo; `minute`:
    /// minutos desde las 00:00 de hoy. Sin horario activado, siempre.
    pub fn allows(&self, weekday: u8, minute: u16) -> bool {
        if !self.enabled {
            return true;
        }
        let day_on = |d: u8| self.days & (1 << (d % 7)) != 0;
        let (from, to) = (self.from_min % DAY_MINUTES, self.to_min % DAY_MINUTES);
        if from < to {
            day_on(weekday) && (from..to).contains(&minute)
        } else {
            // Tramo que cruza la medianoche (o de 24 horas si from == to):
            // la parte de hoy desde `from` y la de madrugada del tramo que
            // empezó ayer, hasta `to`.
            let yesterday = (weekday + 6) % 7;
            (day_on(weekday) && minute >= from) || (day_on(yesterday) && minute < to)
        }
    }
}

/// Día de la semana y minuto del día según el reloj local.
pub fn local_now() -> (u8, u16) {
    use chrono::{Datelike, Local, Timelike};
    let now = Local::now();
    (
        now.weekday().num_days_from_monday() as u8,
        (now.hour() * 60 + now.minute()) as u16,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const MON: u8 = 0;
    const TUE: u8 = 1;
    const SUN: u8 = 6;

    fn at(h: u16, m: u16) -> u16 {
        h * 60 + m
    }

    #[test]
    fn disabled_always_allows() {
        let s = Schedule::default();
        assert!(s.allows(MON, at(12, 0)));
    }

    #[test]
    fn daytime_window_on_chosen_days() {
        let s = Schedule {
            enabled: true,
            days: 0b0000_0011, // lunes y martes
            from_min: at(9, 0),
            to_min: at(17, 30),
        };
        assert!(s.allows(MON, at(9, 0)));
        assert!(s.allows(TUE, at(17, 29)));
        assert!(!s.allows(TUE, at(17, 30)));
        assert!(!s.allows(MON, at(8, 59)));
        assert!(!s.allows(2, at(12, 0))); // miércoles
    }

    #[test]
    fn overnight_window_belongs_to_the_day_it_starts() {
        let s = Schedule {
            enabled: true,
            days: 0b0100_0000, // solo domingo: de 23:00 a 07:00
            from_min: at(23, 0),
            to_min: at(7, 0),
        };
        assert!(s.allows(SUN, at(23, 0)));
        assert!(s.allows(SUN, at(23, 59)));
        assert!(s.allows(MON, at(0, 0))); // madrugada del lunes
        assert!(s.allows(MON, at(6, 59)));
        assert!(!s.allows(MON, at(7, 0)));
        assert!(!s.allows(MON, at(23, 30))); // el lunes no empieza tramo
        assert!(!s.allows(SUN, at(12, 0)));
        assert!(!s.allows(SUN, at(6, 0))); // madrugada del domingo: el tramo del sábado no existe
    }

    #[test]
    fn same_start_and_end_is_a_full_day() {
        let s = Schedule {
            enabled: true,
            days: 0b0000_0001,
            from_min: at(8, 0),
            to_min: at(8, 0),
        };
        assert!(s.allows(MON, at(8, 0)));
        assert!(s.allows(MON, at(23, 59)));
        assert!(s.allows(TUE, at(7, 59)));
        assert!(!s.allows(TUE, at(8, 0)));
        assert!(!s.allows(MON, at(7, 59)));
    }

    #[test]
    fn no_days_means_never() {
        let s = Schedule {
            enabled: true,
            days: 0,
            ..Schedule::default()
        };
        assert!(!s.allows(MON, at(3, 0)));
    }

    #[test]
    fn old_settings_without_schedule_still_load() {
        let s: Schedule = serde_json::from_str("{}").unwrap();
        assert_eq!(s, Schedule::default());
    }
}
