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
    /// Límite de velocidad global: posición en [`SPEED_LIMITS`].
    pub speed_limit: i32,
    /// Conexiones como mucho a un mismo servidor (0 = sin tope).
    pub max_per_server: i32,
    pub watch_clipboard: bool,
    /// Al cerrar la ventana, dejar que `omnid` siga descargando.
    pub keep_downloading: bool,
    /// Aviso al copiar enlaces: 0 completo, 1 compacto, 2 desactivado.
    pub clip_popup: i32,
    /// Cerrar el aviso 1 s después de terminar la comprobación.
    pub clip_popup_autoclose: bool,
    /// "Iniciar al añadir" del aviso completo.
    pub clip_popup_autostart: bool,
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
            speed_limit: 0,
            max_per_server: omni_core::DEFAULT_CONNECTIONS_PER_NETWORK as i32,
            watch_clipboard: true,
            keep_downloading: false,
            clip_popup: 1,
            clip_popup_autoclose: true,
            clip_popup_autostart: true,
        }
    }
}

/// Opciones del límite de velocidad global (KiB/s; 0 = sin límite). El
/// orden coincide con el desplegable "Límite" de la ventana.
pub const SPEED_LIMITS: [u64; 10] = [0, 256, 512, 1024, 2048, 5120, 10240, 20480, 51200, 102400];

/// Límite en bytes por segundo de una posición del desplegable.
pub fn speed_limit_bytes(index: i32) -> Option<u64> {
    let kib = SPEED_LIMITS.get(usize::try_from(index).ok()?).copied()?;
    (kib > 0).then_some(kib * 1024)
}

/// Posición del desplegable que corresponde a un límite: la exacta o, si se
/// puso otro valor desde la terminal, la más cercana.
pub fn speed_limit_index(bytes_per_sec: Option<u64>) -> i32 {
    let Some(b) = bytes_per_sec.filter(|&b| b > 0) else {
        return 0;
    };
    let kib = b / 1024;
    SPEED_LIMITS
        .iter()
        .enumerate()
        .skip(1)
        .min_by_key(|(_, l)| l.abs_diff(kib))
        .map_or(0, |(i, _)| i as i32)
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
        assert_eq!(s.speed_limit, 0);
        assert_eq!(s.max_per_server, 8);
    }

    #[test]
    fn speed_limit_options() {
        assert_eq!(speed_limit_bytes(0), None);
        assert_eq!(speed_limit_bytes(3), Some(1024 * 1024));
        assert_eq!(speed_limit_bytes(99), None);
        assert_eq!(speed_limit_bytes(-1), None);
        assert_eq!(speed_limit_index(None), 0);
        assert_eq!(speed_limit_index(Some(1024 * 1024)), 3);
        // 300 KiB/s no está: se enseña la opción más cercana (256 KB/s).
        assert_eq!(speed_limit_index(Some(300 * 1024)), 1);
        for i in 0..SPEED_LIMITS.len() as i32 {
            assert_eq!(speed_limit_index(speed_limit_bytes(i)), i);
        }
    }
}
