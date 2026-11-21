//! Icono en la bandeja del sistema (área de notificación en Windows, barra de
//! menús en macOS, StatusNotifierItem en Linux).
//!
//! Con el icono puesto, cerrar o minimizar la ventana la esconde en la
//! bandeja: las descargas siguen (las hace `omnid`) y el aviso de enlaces
//! copiados sigue saliendo. Desde el menú del icono se vuelve a abrir la
//! ventana, se pausan o reanudan las descargas y se sale de verdad.
//!
//! Cada sistema va por su lado, todo detrás de la misma [`Tray`]:
//! - Windows y macOS: crate `tray-icon` (nativo: Win32 y AppKit).
//! - Linux y BSD: crate `ksni`, que habla D-Bus directamente, sin GTK. Lo
//!   muestran KDE, XFCE, Cinnamon, MATE, LXQt… y GNOME con la extensión
//!   «AppIndicator». Si el escritorio no tiene bandeja, [`Tray::new`] falla
//!   y la ventana se comporta como antes (cerrar = salir).
//!
//! Los clics llegan como [`TrayEvent`] por un canal que la ventana lee en su
//! temporizador.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

use tokio::runtime::Handle;

/// Lo que pide el usuario desde el icono.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrayEvent {
    /// Clic en el icono o «Abrir OmniLoad».
    Show,
    StartAll,
    PauseAll,
    ToggleClipboard,
    Quit,
}

/// Lo que enseña el icono: texto al pasar el ratón y estado del menú.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrayStatus {
    /// Segunda línea de la ayuda: «2 descargando · 5,1 MiB/s».
    pub tooltip: String,
    /// La cola está en pausa global.
    pub paused: bool,
    pub clipboard: bool,
}

/// El icono de la bandeja. Se quita al soltarlo.
pub struct Tray {
    imp: imp::Tray,
    /// Falso si el escritorio quitó la bandeja (por ejemplo, el panel de
    /// Linux se cerró): entonces la ventana no se puede esconder.
    available: Arc<AtomicBool>,
    last: std::cell::RefCell<Option<TrayStatus>>,
}

impl Tray {
    /// Pone el icono. En macOS hay que llamarlo con el bucle de eventos ya en
    /// marcha (desde un temporizador de Slint, por ejemplo).
    pub fn new(rt: &Handle, events: Sender<TrayEvent>) -> Result<Self, String> {
        let available = Arc::new(AtomicBool::new(true));
        let imp = imp::Tray::new(rt, events, available.clone())?;
        Ok(Self {
            imp,
            available,
            last: Default::default(),
        })
    }

    /// ¿Se ve el icono? Si no, no se debe esconder la ventana.
    pub fn available(&self) -> bool {
        self.available.load(Ordering::Relaxed)
    }

    /// Pone al día la ayuda y el menú (solo si algo cambió).
    pub fn update(&self, status: &TrayStatus) {
        if self.last.borrow().as_ref() == Some(status) {
            return;
        }
        self.imp.update(status);
        *self.last.borrow_mut() = Some(status.clone());
    }
}

/// Ayuda del icono a partir de las descargas activas.
pub fn tooltip(active: usize, waiting: usize, rate: f64, paused: bool) -> String {
    if paused {
        return "Descargas en pausa".into();
    }
    match (active, waiting) {
        (0, 0) => "Sin descargas en curso".into(),
        (0, w) => format!("{w} en espera"),
        (1, _) => format!("1 descargando · {}", crate::format::speed(rate)),
        (a, _) => format!("{a} descargando · {}", crate::format::speed(rate)),
    }
}

/// Icono de la aplicación en RGBA, a 32×32 (lo que usan las bandejas) y a
/// 256×256.
pub fn icon_rgba() -> Vec<(u32, u32, Vec<u8>)> {
    let big = decode_icon().unwrap_or_else(|| (256, 256, fallback_icon(256)));
    let small = downscale(&big.2, big.0, big.1, 32);
    vec![(32, 32, small), big]
}

fn decode_icon() -> Option<(u32, u32, Vec<u8>)> {
    const PNG: &[u8] = include_bytes!("../ui/icon.png");
    let decoder = png::Decoder::new(std::io::Cursor::new(PNG));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    buf.truncate(info.buffer_size());
    Some((info.width, info.height, buf))
}

/// Cuadrado azul por si el PNG no se pudiera leer.
fn fallback_icon(size: u32) -> Vec<u8> {
    [0x25, 0x63, 0xeb, 0xff].repeat((size * size) as usize)
}

/// Reduce una imagen cuadrada RGBA a `to`×`to` promediando bloques (con el
/// color pesado por la opacidad, para que los bordes no salgan oscuros).
fn downscale(rgba: &[u8], w: u32, h: u32, to: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((to * to * 4) as usize);
    for oy in 0..to {
        for ox in 0..to {
            let (x0, x1) = (ox * w / to, ((ox + 1) * w / to).max(ox * w / to + 1));
            let (y0, y1) = (oy * h / to, ((oy + 1) * h / to).max(oy * h / to + 1));
            let mut acc = [0u64; 4];
            let mut n = 0u64;
            for y in y0..y1.min(h) {
                for x in x0..x1.min(w) {
                    let i = ((y * w + x) * 4) as usize;
                    let a = rgba[i + 3] as u64;
                    acc[0] += rgba[i] as u64 * a;
                    acc[1] += rgba[i + 1] as u64 * a;
                    acc[2] += rgba[i + 2] as u64 * a;
                    acc[3] += a;
                    n += 1;
                }
            }
            let px = match acc[0..3]
                .iter()
                .map(|c| c.checked_div(acc[3]))
                .collect::<Option<Vec<_>>>()
            {
                Some(rgb) => [
                    rgb[0] as u8,
                    rgb[1] as u8,
                    rgb[2] as u8,
                    (acc[3] / n.max(1)) as u8,
                ],
                None => [0; 4],
            };
            out.extend_from_slice(&px);
        }
    }
    out
}

/// Textos del menú, iguales en todos los sistemas.
mod labels {
    pub const OPEN: &str = "Abrir OmniLoad";
    pub const START: &str = "Iniciar descargas";
    pub const PAUSE: &str = "Pausar descargas";
    pub const CLIPBOARD: &str = "Vigilar portapapeles";
    pub const QUIT: &str = "Salir";
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
mod imp {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::Sender;

    use tokio::runtime::Handle;
    use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{
        Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    };

    use super::{TrayEvent, TrayStatus, labels};

    pub struct Tray {
        icon: TrayIcon,
        start: MenuItem,
        pause: MenuItem,
        clipboard: CheckMenuItem,
    }

    impl Tray {
        pub fn new(
            _rt: &Handle,
            events: Sender<TrayEvent>,
            _available: Arc<AtomicBool>,
        ) -> Result<Self, String> {
            let open = MenuItem::with_id("abrir", labels::OPEN, true, None);
            let start = MenuItem::with_id("iniciar", labels::START, true, None);
            let pause = MenuItem::with_id("pausar", labels::PAUSE, true, None);
            let clipboard =
                CheckMenuItem::with_id("portapapeles", labels::CLIPBOARD, true, true, None);
            let quit = MenuItem::with_id("salir", labels::QUIT, true, None);
            let menu = Menu::new();
            menu.append_items(&[
                &open,
                &PredefinedMenuItem::separator(),
                &start,
                &pause,
                &clipboard,
                &PredefinedMenuItem::separator(),
                &quit,
            ])
            .map_err(|e| e.to_string())?;

            // Los manejadores pueden llamarse desde otro hilo: el canal va
            // dentro de un Mutex para que sean Send + Sync.
            let tx = Mutex::new(events.clone());
            MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
                let ev = match e.id.0.as_str() {
                    "abrir" => TrayEvent::Show,
                    "iniciar" => TrayEvent::StartAll,
                    "pausar" => TrayEvent::PauseAll,
                    "portapapeles" => TrayEvent::ToggleClipboard,
                    "salir" => TrayEvent::Quit,
                    _ => return,
                };
                if let Ok(tx) = tx.lock() {
                    let _ = tx.send(ev);
                }
            }));
            let tx = Mutex::new(events);
            TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
                let show = matches!(
                    e,
                    TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } | TrayIconEvent::DoubleClick {
                        button: MouseButton::Left,
                        ..
                    }
                );
                if show && let Ok(tx) = tx.lock() {
                    let _ = tx.send(TrayEvent::Show);
                }
            }));

            let (w, h, rgba) = super::icon_rgba().swap_remove(0);
            let icon = Icon::from_rgba(rgba, w, h).map_err(|e| e.to_string())?;
            let builder = TrayIconBuilder::new()
                .with_id("omniload")
                .with_menu(Box::new(menu))
                .with_tooltip("OmniLoad")
                .with_icon(icon);
            // En Windows el clic izquierdo abre la ventana y el derecho el
            // menú; en macOS lo normal es que cualquier clic abra el menú.
            #[cfg(target_os = "windows")]
            let builder = builder.with_menu_on_left_click(false);
            let icon = builder.build().map_err(|e| e.to_string())?;
            Ok(Self {
                icon,
                start,
                pause,
                clipboard,
            })
        }

        pub fn update(&self, s: &TrayStatus) {
            let tip = if s.tooltip.is_empty() {
                "OmniLoad".to_owned()
            } else {
                format!("OmniLoad\n{}", s.tooltip)
            };
            let _ = self.icon.set_tooltip(Some(tip));
            self.start.set_enabled(s.paused);
            self.pause.set_enabled(!s.paused);
            self.clipboard.set_checked(s.clipboard);
        }
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
mod imp {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::Sender;

    use ksni::menu::{CheckmarkItem, StandardItem};
    use ksni::{MenuItem, TrayMethods};
    use tokio::runtime::Handle;

    use super::{TrayEvent, TrayStatus, labels};

    struct Sni {
        events: Sender<TrayEvent>,
        status: TrayStatus,
        icons: Vec<ksni::Icon>,
        available: Arc<AtomicBool>,
    }

    impl Sni {
        fn send(&self, e: TrayEvent) {
            let _ = self.events.send(e);
        }
    }

    impl ksni::Tray for Sni {
        fn id(&self) -> String {
            "omniload".into()
        }

        fn title(&self) -> String {
            "OmniLoad".into()
        }

        fn category(&self) -> ksni::Category {
            ksni::Category::ApplicationStatus
        }

        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            self.icons.clone()
        }

        fn tool_tip(&self) -> ksni::ToolTip {
            ksni::ToolTip {
                title: "OmniLoad".into(),
                description: self.status.tooltip.clone(),
                ..Default::default()
            }
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            self.send(TrayEvent::Show);
        }

        fn menu(&self) -> Vec<MenuItem<Self>> {
            let item = |label: &str, enabled: bool, ev: TrayEvent| -> MenuItem<Self> {
                StandardItem {
                    label: label.into(),
                    enabled,
                    activate: Box::new(move |t: &mut Self| t.send(ev.clone())),
                    ..Default::default()
                }
                .into()
            };
            vec![
                item(labels::OPEN, true, TrayEvent::Show),
                MenuItem::Separator,
                item(labels::START, self.status.paused, TrayEvent::StartAll),
                item(labels::PAUSE, !self.status.paused, TrayEvent::PauseAll),
                CheckmarkItem {
                    label: labels::CLIPBOARD.into(),
                    checked: self.status.clipboard,
                    activate: Box::new(|t: &mut Self| t.send(TrayEvent::ToggleClipboard)),
                    ..Default::default()
                }
                .into(),
                MenuItem::Separator,
                item(labels::QUIT, true, TrayEvent::Quit),
            ]
        }

        fn watcher_online(&self) {
            self.available.store(true, Ordering::Relaxed);
        }

        fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
            // Seguir esperando a que vuelva la bandeja; mientras, la ventana
            // no se esconde.
            self.available.store(false, Ordering::Relaxed);
            true
        }
    }

    pub struct Tray {
        rt: Handle,
        handle: ksni::Handle<Sni>,
    }

    impl Tray {
        pub fn new(
            rt: &Handle,
            events: Sender<TrayEvent>,
            available: Arc<AtomicBool>,
        ) -> Result<Self, String> {
            let icons = super::icon_rgba()
                .into_iter()
                .map(|(w, h, rgba)| ksni::Icon {
                    width: w as i32,
                    height: h as i32,
                    // StatusNotifierItem quiere ARGB en orden de red.
                    data: rgba
                        .chunks_exact(4)
                        .flat_map(|p| [p[3], p[0], p[1], p[2]])
                        .collect(),
                })
                .collect();
            let sni = Sni {
                events,
                status: TrayStatus::default(),
                icons,
                available,
            };
            let handle = rt.block_on(sni.spawn()).map_err(|e| e.to_string())?;
            Ok(Self {
                rt: rt.clone(),
                handle,
            })
        }

        pub fn update(&self, s: &TrayStatus) {
            let handle = self.handle.clone();
            let s = s.clone();
            self.rt.spawn(async move {
                handle.update(move |t| t.status = s).await;
            });
        }
    }

    impl Drop for Tray {
        fn drop(&mut self) {
            let handle = self.handle.clone();
            // Quitar el icono al salir (sin esperar mucho).
            let _ = self.rt.block_on(async {
                tokio::time::timeout(std::time::Duration::from_secs(1), handle.shutdown()).await
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_decodes_and_scales() {
        let icons = icon_rgba();
        assert_eq!(icons.len(), 2);
        let (w, h, small) = &icons[0];
        assert_eq!((*w, *h), (32, 32));
        assert_eq!(small.len(), 32 * 32 * 4);
        let (w, h, big) = &icons[1];
        assert_eq!((*w, *h), (256, 256));
        assert_eq!(big.len(), 256 * 256 * 4);
        // El centro del icono es azul y opaco (no el cuadrado de reserva:
        // el PNG se leyó bien si hay píxeles blancos de la flecha).
        assert!(big.chunks_exact(4).any(|p| p == [255, 255, 255, 255]));
        assert_eq!(small[(16 * 32 + 2) * 4 + 3], 255);
    }

    #[test]
    fn downscale_keeps_transparent_edges_clean() {
        // 2×2 → 1×1: un píxel rojo opaco y tres transparentes negros.
        let px = [255, 0, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(downscale(&px, 2, 2, 1), vec![255, 0, 0, 63]);
    }

    #[test]
    fn tooltips() {
        assert_eq!(tooltip(0, 0, 0.0, false), "Sin descargas en curso");
        assert_eq!(tooltip(0, 4, 0.0, false), "4 en espera");
        assert_eq!(tooltip(2, 1, 1536.0, false), "2 descargando · 1,5 KiB/s");
        assert_eq!(tooltip(2, 1, 1536.0, true), "Descargas en pausa");
    }
}
