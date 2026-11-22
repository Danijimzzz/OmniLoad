//! Ajustes de la ventana, guardados en `ajustes.json` dentro de la carpeta de
//! configuración del sistema (`%APPDATA%\OmniLoad` en Windows,
//! `~/.config/omniload` en Linux, `~/Library/Application Support/OmniLoad` en
//! macOS).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Todo lo que el usuario elige en Ajustes y en la barra inferior.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// 0 Cristal claro, 1 Cristal oscuro, 2 Claro, 3 Grafito, 4 Alto
    /// contraste.
    pub theme: i32,
    /// Cambiar entre claro y oscuro según el sistema.
    pub follow_system: bool,
    /// Panel de propiedades: 0 abajo, 1 a la derecha, 2 oculto.
    pub properties_panel: i32,
    pub compact: bool,
    /// 0 azul, 1 violeta, 2 verde, 3 naranja, 4 rosa, 5 grafito.
    pub accent: i32,
    pub reduce_transparency: bool,
    /// Carpeta base de las descargas.
    pub download_dir: PathBuf,
    /// Guardar cada paquete en una subcarpeta con su nombre.
    pub package_folders: bool,
    /// También los paquetes de un solo archivo.
    pub single_file_folders: bool,
    pub max_active: i32,
    pub connections: i32,
    /// Conexiones como mucho a un mismo servidor (0 = sin tope).
    pub max_per_server: i32,
    pub watch_clipboard: bool,
    /// Al salir de OmniLoad, dejar que `omnid` siga descargando.
    pub keep_downloading: bool,
    /// Cerrar la ventana la esconde en la bandeja (si hay bandeja).
    pub close_to_tray: bool,
    /// Minimizar la ventana también la esconde en la bandeja.
    pub minimize_to_tray: bool,
    /// Notificación del sistema al terminar descargas.
    pub notify_done: bool,
    /// Al abrir, mirar en GitHub si hay una versión nueva (solo avisa).
    pub check_updates: bool,
    /// Ya se avisó una vez de que OmniLoad sigue en la bandeja.
    pub tray_hint_shown: bool,
    /// Aviso al copiar enlaces: 0 completo, 1 compacto, 2 desactivado.
    pub clip_popup: i32,
    /// Desvanecer y cerrar el aviso unos segundos después de terminar la comprobación.
    pub clip_popup_autoclose: bool,
    /// "Iniciar al añadir" del aviso completo.
    pub clip_popup_autostart: bool,
    /// Reglas por tipo de archivo: extensiones → carpeta.
    pub file_rules: Vec<RuleCfg>,
    /// Horario de descargas (se pasa a la cola del servicio).
    pub schedule_enabled: bool,
    /// Días del horario: bit 0 lunes … bit 6 domingo.
    pub schedule_days: i32,
    pub schedule_from: String,
    pub schedule_to: String,
    /// Al terminar todo: 0 nada, 1 cerrar OmniLoad, 2 suspender, 3 apagar.
    pub finish_action: i32,
}

/// Una regla por tipo de archivo, tal como se escribe en Ajustes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleCfg {
    /// Extensiones separadas por comas o espacios («mp4, mkv»).
    pub extensions: String,
    pub folder: PathBuf,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: 0,
            follow_system: true,
            properties_panel: 0,
            compact: false,
            accent: 0,
            reduce_transparency: false,
            download_dir: default_download_dir(),
            package_folders: true,
            single_file_folders: true,
            max_active: 3,
            connections: omni_core::DEFAULT_CONNECTIONS_PER_NETWORK as i32,
            max_per_server: omni_core::DEFAULT_CONNECTIONS_PER_NETWORK as i32,
            watch_clipboard: true,
            keep_downloading: false,
            close_to_tray: true,
            minimize_to_tray: true,
            notify_done: true,
            check_updates: true,
            tray_hint_shown: false,
            clip_popup: 1,
            clip_popup_autoclose: true,
            clip_popup_autostart: true,
            file_rules: Vec::new(),
            schedule_enabled: false,
            schedule_days: i32::from(omni_queue::ALL_DAYS),
            schedule_from: "00:00".into(),
            schedule_to: "07:00".into(),
            finish_action: 0,
        }
    }
}

/// «HH:MM» (también «H:MM» o «HH.MM») a minutos desde las 00:00.
pub fn parse_hhmm(text: &str) -> Option<u16> {
    let (h, m) = text.trim().split_once([':', '.'])?;
    let (h, m): (u16, u16) = (h.trim().parse().ok()?, m.trim().parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

impl Settings {
    /// El horario tal como lo entiende la cola, o `None` si las horas no son
    /// válidas.
    pub fn schedule(&self) -> Option<omni_queue::Schedule> {
        Some(omni_queue::Schedule {
            enabled: self.schedule_enabled,
            days: (self.schedule_days & i32::from(omni_queue::ALL_DAYS)) as u8,
            from_min: parse_hhmm(&self.schedule_from)?,
            to_min: parse_hhmm(&self.schedule_to)?,
        })
    }
}

/// Valores del desplegable «Límite» (KiB/s; 0 = sin límite). Además se puede
/// escribir cualquier otro.
pub const SPEED_LIMITS: [u64; 10] = [0, 256, 512, 1024, 2048, 5120, 10240, 20480, 51200, 102400];

/// Límite en bytes por segundo de una posición del desplegable.
pub fn speed_limit_bytes(index: i32) -> Option<u64> {
    let kib = SPEED_LIMITS.get(usize::try_from(index).ok()?).copied()?;
    (kib > 0).then_some(kib * 1024)
}

/// Lo que se escribe en el cuadro del límite: «Sin límite», «700 KB/s» o
/// «3 MB/s».
pub fn speed_text(limit: Option<u64>) -> String {
    let Some(b) = limit.filter(|&b| b > 0) else {
        return "Sin límite".into();
    };
    let kib = b / 1024;
    if kib >= 1024 && kib % 1024 == 0 {
        format!("{} MB/s", kib / 1024)
    } else if kib >= 1 {
        format!("{kib} KB/s")
    } else {
        format!("{b} B/s")
    }
}

/// Entiende lo que escribe el usuario: «700», «700 KB/s», «3 mb», «1,5 M»,
/// «0» o «sin límite». Un número solo son KB/s. `Err` si no se entiende.
pub fn parse_speed(text: &str) -> Result<Option<u64>, ()> {
    let t = text.trim().to_lowercase();
    if t.is_empty() || t.starts_with("sin") || t.starts_with("no") || t == "∞" {
        return Ok(None);
    }
    let t = t.trim_end_matches("/s").trim_end_matches("ps").trim();
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == ',' || c == '.'))
        .unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let value: f64 = num.replace(',', ".").parse().map_err(|_| ())?;
    let mult = match unit.trim().trim_end_matches('b').trim_end_matches('i') {
        "" | "k" => 1024.0,
        "m" => 1024.0 * 1024.0,
        "g" => 1024.0 * 1024.0 * 1024.0,
        _ => return Err(()),
    };
    if !value.is_finite() || value < 0.0 {
        return Err(());
    }
    let bytes = (value * mult) as u64;
    Ok((bytes > 0).then_some(bytes))
}

impl Settings {
    /// Lee los ajustes; si no hay o están rotos, los de por defecto.
    pub fn load() -> Self {
        let Some(path) = path() else {
            return Self::default();
        };
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!("ajustes ilegibles en {}: {e}", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Guarda los ajustes (escribe a un temporal y lo renombra, para no dejar
    /// el archivo a medias).
    pub fn save(&self) {
        let Some(path) = path() else { return };
        let result = (|| -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
            std::fs::rename(&tmp, &path)
        })();
        if let Err(e) = result {
            tracing::warn!("no se pudieron guardar los ajustes: {e}");
        }
    }
}

/// `ventana.json` (ver [`crate::single`]): junto a los ajustes.
pub fn instance_path() -> Option<PathBuf> {
    path().map(|p| p.with_file_name("ventana.json"))
}

fn path() -> Option<PathBuf> {
    // Para pruebas y para tener varias configuraciones a la vez.
    if let Some(p) = std::env::var_os("OMNILOAD_AJUSTES") {
        return Some(p.into());
    }
    let dir = if cfg!(any(target_os = "windows", target_os = "macos")) {
        "OmniLoad"
    } else {
        "omniload"
    };
    dirs::config_dir().map(|d| d.join(dir).join("ajustes.json"))
}

/// La carpeta de descargas del usuario, o la actual si no hay.
fn default_download_dir() -> PathBuf {
    dirs::download_dir()
        .or_else(dirs::home_dir)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_take_defaults() {
        let s: Settings = serde_json::from_str(r#"{"theme": 3, "compact": true}"#).unwrap();
        assert_eq!(s.theme, 3);
        assert!(s.compact);
        assert_eq!(s.max_active, 3);
        assert!(s.watch_clipboard);
        assert!(s.package_folders);
        assert_eq!(s.clip_popup, 1);
        assert!(s.clip_popup_autoclose);
        assert_eq!(s.max_per_server, 8);
        assert!(s.close_to_tray);
        assert!(s.minimize_to_tray);
        assert!(s.notify_done);
        assert!(s.check_updates);
        assert!(!s.tray_hint_shown);
    }

    #[test]
    fn times_parse_and_format() {
        assert_eq!(parse_hhmm("07:30"), Some(450));
        assert_eq!(parse_hhmm(" 7.05 "), Some(425));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("12:60"), None);
        assert_eq!(parse_hhmm("abc"), None);
        let mut s = Settings::default();
        assert!(s.schedule().is_some());
        s.schedule_to = "mal".into();
        assert!(s.schedule().is_none());
    }

    #[test]
    fn speed_limit_text_round_trip() {
        assert_eq!(speed_limit_bytes(0), None);
        assert_eq!(speed_limit_bytes(3), Some(1024 * 1024));
        assert_eq!(speed_limit_bytes(99), None);
        assert_eq!(parse_speed("Sin límite"), Ok(None));
        assert_eq!(parse_speed("0"), Ok(None));
        assert_eq!(parse_speed("700"), Ok(Some(700 * 1024)));
        assert_eq!(parse_speed("700 KB/s"), Ok(Some(700 * 1024)));
        assert_eq!(parse_speed("3 mb/s"), Ok(Some(3 * 1024 * 1024)));
        assert_eq!(parse_speed("1,5 M"), Ok(Some(1536 * 1024)));
        assert_eq!(parse_speed("MiB"), Err(()));
        assert_eq!(parse_speed("abc"), Err(()));
        assert_eq!(parse_speed("5 xb"), Err(()));
        assert_eq!(speed_text(None), "Sin límite");
        assert_eq!(speed_text(Some(3 * 1024 * 1024)), "3 MB/s");
        assert_eq!(speed_text(Some(700 * 1024)), "700 KB/s");
        for l in [None, Some(700 * 1024), Some(2 * 1024 * 1024)] {
            assert_eq!(parse_speed(&speed_text(l)), Ok(l));
        }
    }
}
