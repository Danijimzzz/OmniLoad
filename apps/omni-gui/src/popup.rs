//! El aviso de enlaces copiados: una ventanita sin marco, siempre encima y sin
//! icono en la barra de tareas, abajo a la derecha de la pantalla. Enseña en
//! vivo los enlaces que se van copiando mientras se comprueban.
//!
//! Aquí va lo que no depende de Slint (estado, textos y la rueda) y lo que
//! toca winit para crear y colocar la ventana.

use std::cell::Cell;
use std::time::{Duration, Instant};

use slint::winit_030::winit;

use omni_grab::{Check, GrabItem};

/// Lo que pasa con la tanda de enlaces que enseña el aviso.
#[derive(Debug, Default)]
pub struct State {
    /// Tanda actual (0 = aviso cerrado).
    pub batch: u64,
    pub compact: bool,
    started: Option<Instant>,
    /// Cuándo terminó la comprobación, y cuánto tardó.
    done: Option<(Instant, Duration)>,
}

/// Recuento de una tanda.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub total: usize,
    pub ok: usize,
    pub bad: usize,
    pub checking: usize,
    /// Disponibles y marcados: los que se descargarían.
    pub chosen: usize,
}

pub fn counts(items: &[GrabItem]) -> Counts {
    let mut c = Counts {
        total: items.len(),
        ..Counts::default()
    };
    for i in items {
        match i.check {
            Check::Checking => c.checking += 1,
            Check::Online { .. } => {
                c.ok += 1;
                if i.selected {
                    c.chosen += 1;
                }
            }
            Check::Offline(_) => c.bad += 1,
        }
    }
    c
}

impl State {
    pub fn is_open(&self) -> bool {
        self.batch != 0
    }

    pub fn open(&mut self, batch: u64, compact: bool, now: Instant) {
        *self = State {
            batch,
            compact,
            started: Some(now),
            done: None,
        };
    }

    pub fn close(&mut self) {
        *self = State::default();
    }

    /// Llegaron más enlaces a la tanda: vuelve a estar comprobando.
    pub fn more_links(&mut self) {
        self.done = None;
    }

    /// Actualiza con el recuento de ahora. Devuelve el texto del tiempo.
    pub fn tick(&mut self, c: &Counts, now: Instant) -> String {
        let started = *self.started.get_or_insert(now);
        if c.checking > 0 {
            self.done = None;
            return format!("Comprobando… {}", seconds(now - started));
        }
        let (_, took) = *self.done.get_or_insert((now, now - started));
        format!("Comprobado en {}", seconds(took))
    }

    /// Si toca cerrarlo solo: 1 s después de terminar de comprobar.
    pub fn should_autoclose(&self, now: Instant) -> bool {
        self.done
            .is_some_and(|(at, _)| now.duration_since(at) >= Duration::from_secs(1))
    }
}

/// "2,4 s" o, a partir del minuto, "1 min 5 s".
fn seconds(d: Duration) -> String {
    if d < Duration::from_secs(60) {
        format!("{:.1} s", d.as_secs_f64()).replace('.', ",")
    } else {
        crate::format::duration(d)
    }
}

/// Los arcos verde (aptos) y rojo (caídos) de la rueda, en un cuadro de
/// 44×44 con radio 18, empezando arriba y en el sentido del reloj.
pub fn ring_arcs(ok: usize, bad: usize, total: usize) -> (String, String) {
    if total == 0 {
        return (String::new(), String::new());
    }
    let ok_end = ok as f64 / total as f64;
    let bad_end = (ok + bad) as f64 / total as f64;
    (arc(0.0, ok_end), arc(ok_end, bad_end))
}

fn arc(from: f64, to: f64) -> String {
    const C: f64 = 22.0;
    const R: f64 = 18.0;
    if to - from <= 0.0 {
        return String::new();
    }
    let point = |f: f64| {
        let a = f * std::f64::consts::TAU - std::f64::consts::FRAC_PI_2;
        (C + R * a.cos(), C + R * a.sin())
    };
    if to - from >= 0.9999 {
        // Una vuelta entera: dos medias vueltas (un arco no puede cerrarse).
        return format!(
            "M {C} {} A {R} {R} 0 1 1 {C} {} A {R} {R} 0 1 1 {C} {}",
            C - R,
            C + R,
            C - R
        );
    }
    let (x0, y0) = point(from);
    let (x1, y1) = point(to);
    let large = u8::from(to - from > 0.5);
    format!("M {x0:.2} {y0:.2} A {R} {R} 0 {large} 1 {x1:.2} {y1:.2}")
}

/// Texto y tono del estado de un enlace en el aviso.
pub fn status(item: &GrabItem) -> (&'static str, bool, i32) {
    // (texto, disponible, tono: 1 bien, 2 aviso, 3 error, 4 apagado)
    if item.duplicate && matches!(item.check, Check::Online { .. }) {
        return ("Ya en Descargas", true, 2);
    }
    match &item.check {
        Check::Checking => ("Comprobando…", false, 4),
        Check::Online { .. } => ("Disponible", true, 1),
        Check::Offline(_) => ("Caído", false, 3),
    }
}

thread_local! {
    /// La próxima ventana que se cree es la del aviso.
    static NEXT_IS_POPUP: Cell<bool> = const { Cell::new(false) };
}

/// Elige el motor de ventanas (winit) con un gancho que da a la ventana del
/// aviso sus atributos: sin marco, siempre encima, sin robar el foco y sin
/// icono en la barra de tareas.
pub fn select_backend() {
    let result = slint::BackendSelector::new()
        .with_winit_window_attributes_hook(|attrs| {
            if NEXT_IS_POPUP.with(Cell::get) {
                popup_attributes(attrs)
            } else {
                attrs
            }
        })
        .select();
    if let Err(e) = result {
        tracing::warn!("no se pudo preparar el aviso de enlaces: {e}");
    }
}

/// Crea la ventana del aviso con los atributos de [`select_backend`].
pub fn create<T>(make: impl FnOnce() -> T) -> T {
    NEXT_IS_POPUP.with(|f| f.set(true));
    let window = make();
    NEXT_IS_POPUP.with(|f| f.set(false));
    window
}

fn popup_attributes(attrs: winit::window::WindowAttributes) -> winit::window::WindowAttributes {
    #[allow(unused_mut)]
    let mut attrs = attrs
        .with_active(false)
        .with_decorations(false)
        .with_window_level(winit::window::WindowLevel::AlwaysOnTop);
    #[cfg(target_os = "windows")]
    {
        use winit::platform::windows::WindowAttributesExtWindows;
        attrs = attrs.with_skip_taskbar(true);
    }
    #[cfg(target_os = "linux")]
    {
        use winit::platform::x11::{WindowAttributesExtX11, WindowType};
        attrs = attrs.with_x11_window_type(vec![WindowType::Notification]);
    }
    attrs
}

/// Dónde va el aviso: esquina de abajo a la derecha del monitor, dejando sitio
/// a la barra de tareas o al Dock. En Wayland el sistema no deja elegir la
/// posición y lo coloca él.
pub fn corner(
    monitor: &winit::monitor::MonitorHandle,
    size: slint::PhysicalSize,
) -> slint::PhysicalPosition {
    let scale = monitor.scale_factor();
    let px = |l: f64| (l * scale).round() as i32;
    let bottom = if cfg!(target_os = "windows") {
        px(60.0)
    } else if cfg!(target_os = "macos") {
        px(90.0)
    } else {
        px(56.0)
    };
    let pos = monitor.position();
    let area = monitor.size();
    slint::PhysicalPosition::new(
        pos.x + area.width as i32 - size.width as i32 - px(16.0),
        pos.y + area.height as i32 - size.height as i32 - bottom,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: u64, check: Check, selected: bool) -> GrabItem {
        GrabItem {
            id,
            name: format!("{id}.zip"),
            host: "a.com".into(),
            url: format!("https://a.com/{id}.zip"),
            size: None,
            check,
            selected,
            duplicate: false,
            batch: 1,
        }
    }

    #[test]
    fn counts_and_ring() {
        let items = [
            item(1, Check::Online { resumable: true }, true),
            item(2, Check::Online { resumable: true }, false),
            item(3, Check::Offline("x".into()), false),
            item(4, Check::Checking, true),
        ];
        let c = counts(&items);
        assert_eq!(
            c,
            Counts {
                total: 4,
                ok: 2,
                bad: 1,
                checking: 1,
                chosen: 1
            }
        );
        let (ok, bad) = ring_arcs(c.ok, c.bad, c.total);
        // Verde: media vuelta, de arriba a abajo.
        assert_eq!(ok, "M 22.00 4.00 A 18 18 0 0 1 22.00 40.00");
        // Rojo: un cuarto más, hasta la izquierda.
        assert_eq!(bad, "M 22.00 40.00 A 18 18 0 0 1 4.00 22.00");
        assert_eq!(ring_arcs(0, 0, 0), (String::new(), String::new()));
        let (full, none) = ring_arcs(3, 0, 3);
        assert!(full.matches(" A ").count() == 2, "{full}");
        assert!(none.is_empty());
    }

    #[test]
    fn time_and_autoclose() {
        let t0 = Instant::now();
        let mut st = State::default();
        st.open(5, true, t0);
        assert!(st.is_open());
        let busy = Counts {
            total: 2,
            checking: 1,
            ..Counts::default()
        };
        assert_eq!(
            st.tick(&busy, t0 + Duration::from_millis(1500)),
            "Comprobando… 1,5 s"
        );
        assert!(!st.should_autoclose(t0 + Duration::from_secs(10)));

        let done = Counts {
            total: 2,
            ok: 2,
            ..Counts::default()
        };
        let t1 = t0 + Duration::from_millis(2400);
        assert_eq!(st.tick(&done, t1), "Comprobado en 2,4 s");
        // El tiempo se queda fijo.
        assert_eq!(
            st.tick(&done, t1 + Duration::from_secs(3)),
            "Comprobado en 2,4 s"
        );
        assert!(!st.should_autoclose(t1 + Duration::from_millis(500)));
        assert!(st.should_autoclose(t1 + Duration::from_secs(1)));

        st.more_links();
        assert!(!st.should_autoclose(t1 + Duration::from_secs(5)));
        st.close();
        assert!(!st.is_open());
    }
}
