//! Archivo de estado para reanudar: `<destino>.part.omni`, en JSON.
//!
//! En la fase 2 este estado pasará a SQLite (crate omni-store), pero el
//! contenido es el mismo: tamaño, validadores y bloques con lo escrito.

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
    append_ext(dest, ".part.omni")
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
        assert!(path.to_string_lossy().ends_with("a.bin.part.omni"));
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
    fn corrupt_state_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.part.omni");
        fs::write(&path, b"{no es json").unwrap();
        assert!(SavedState::load(&path).is_none());
    }
}
