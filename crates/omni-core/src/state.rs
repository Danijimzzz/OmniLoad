//! Archivo de estado para reanudar, en JSON: tamaño, validadores y bloques
//! con lo escrito.
//!
//! No va junto a la descarga (antes era `<destino>.part.omni`): como se
//! guarda cada poco con un temporal que se renombra, en la carpeta se veía un
//! archivo aparecer y desaparecer sin parar. Ahora vive en la carpeta de datos
//! del programa (`%LOCALAPPDATA%\OmniLoad\reanudar` en Windows,
//! `~/.local/share/omniload/reanudar` en Linux,
//! `~/Library/Application Support/OmniLoad/reanudar` en macOS), con un nombre
//! sacado de la ruta del destino. En la carpeta de descargas solo queda el
//! `.part`. La variable `OMNILOAD_REANUDAR` cambia esa carpeta.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::blocks::Block;

const VERSION: u32 = 1;

/// Ruta del archivo `.part` de un destino.
pub fn part_path_for(dest: &Path) -> PathBuf {
    append_ext(dest, ".part")
}

/// Ruta del archivo de estado de un destino.
pub fn state_path_for(dest: &Path) -> PathBuf {
    match state_dir() {
        Some(dir) => dir.join(state_file_name(dest)),
        // Sin carpeta de datos (raro): junto a la descarga, como antes.
        None => legacy_state_path_for(dest),
    }
}

/// Dónde iba el estado en versiones anteriores: junto a la descarga. Se lee
/// para no perder lo que estuviera a medias al actualizar.
pub fn legacy_state_path_for(dest: &Path) -> PathBuf {
    append_ext(dest, ".part.omni")
}

fn state_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("OMNILOAD_REANUDAR") {
        return Some(dir.into());
    }
    let app = if cfg!(any(target_os = "windows", target_os = "macos")) {
        "OmniLoad"
    } else {
        "omniload"
    };
    dirs::data_local_dir().map(|d| d.join(app).join("reanudar"))
}

/// `<nombre>-<huella de la ruta>.json`: el nombre para reconocerlo a ojo, la
/// huella para que dos descargas con el mismo nombre en carpetas distintas no
/// se pisen.
fn state_file_name(dest: &Path) -> String {
    use sha2::{Digest, Sha256};
    let abs = std::path::absolute(dest).unwrap_or_else(|_| dest.to_path_buf());
    let hash = Sha256::digest(abs.as_os_str().as_encoded_bytes());
    let name: String = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .take(60)
        .collect();
    format!("{name}-{}.json", hex::encode(&hash[..10]))
}

fn append_ext(dest: &Path, ext: &str) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(ext);
    PathBuf::from(s)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedState {
    pub version: u32,
    pub url: String,
    pub size: u64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub blocks: Vec<Block>,
}

impl SavedState {
    pub fn new(
        url: String,
        size: u64,
        etag: Option<String>,
        last_modified: Option<String>,
        blocks: Vec<Block>,
    ) -> Self {
        Self {
            version: VERSION,
            url,
            size,
            etag,
            last_modified,
            blocks,
        }
    }

    pub fn written(&self) -> u64 {
        self.blocks.iter().map(|b| b.written).sum()
    }

    /// Lee el estado; `None` si no existe o está dañado.
    pub fn load(path: &Path) -> Option<Self> {
        let data = fs::read(path).ok()?;
        let state: Self = serde_json::from_slice(&data).ok()?;
        (state.version == VERSION).then_some(state)
    }

    /// Guarda de forma atómica: escribe un temporal, lo sincroniza y lo
    /// renombra encima. Un corte a mitad deja el estado anterior intacto.
    pub fn save_atomic(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(dir)?;
        }
        let tmp = append_ext(path, ".tmp");
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&serde_json::to_vec(self).map_err(io::Error::other)?)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)
    }

    /// ¿Sirve este estado para el archivo remoto actual? El tamaño debe
    /// coincidir, y también ETag y Last-Modified cuando existan.
    pub fn matches(&self, size: u64, etag: Option<&str>, last_modified: Option<&str>) -> bool {
        let same = |saved: &Option<String>, now: Option<&str>| match (saved.as_deref(), now) {
            (Some(a), Some(b)) => a == b,
            (None, None) => true,
            // Si el servidor dejó de mandar un validador (o empezó a
            // mandarlo) no podemos asegurar que sea el mismo archivo.
            _ => false,
        };
        self.size == size && same(&self.etag, etag) && same(&self.last_modified, last_modified)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path_for(&dir.path().join("a.bin"));
        // Fuera de la carpeta de descargas, con el nombre delante.
        assert!(!path.starts_with(dir.path()));
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("a.bin-") && name.ends_with(".json"),
            "{name}"
        );
        // El mismo nombre en otra carpeta no se pisa.
        assert_ne!(path, state_path_for(&dir.path().join("otra").join("a.bin")));
        let s = SavedState::new(
            "http://x/a.bin".into(),
            10,
            Some("\"e1\"".into()),
            None,
            vec![Block {
                start: 0,
                end: 10,
                written: 4,
            }],
        );
        // Se guarda en la carpeta temporal, no en la de datos del usuario.
        let path = dir.path().join("estado").join("a.json");
        s.save_atomic(&path).unwrap();
        s.save_atomic(&path).unwrap();
        let loaded = SavedState::load(&path).unwrap();
        assert_eq!(loaded, s);
        assert!(loaded.matches(10, Some("\"e1\""), None));
        assert!(!loaded.matches(10, Some("\"e2\""), None));
        assert!(!loaded.matches(11, Some("\"e1\""), None));
        assert!(!loaded.matches(10, None, None));
    }

    #[test]
    fn odd_names_are_cleaned() {
        let name = state_file_name(Path::new("/x/a b?:*<>|ñ.zip"));
        assert!(name.starts_with("abñ.zip-"), "{name}");
    }

    #[test]
    fn corrupt_state_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.part.omni");
        fs::write(&path, b"{no es json").unwrap();
        assert!(SavedState::load(&path).is_none());
    }
}
