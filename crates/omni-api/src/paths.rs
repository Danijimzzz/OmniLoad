//! Dónde viven la base de datos, el archivo de contacto y el candado.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Archivos del servicio. Todos van en la misma carpeta que la base de datos.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    pub db: PathBuf,
    /// Puerto y token del servicio en marcha.
    pub endpoint: PathBuf,
    /// Candado: solo un servicio por base de datos.
    pub lock: PathBuf,
    /// Registro del servicio cuando se lanza en segundo plano.
    pub log: PathBuf,
}

impl Paths {
    /// Los de siempre (ver [`omni_store::Store::default_path`]; la variable
    /// `OMNILOAD_DB` los cambia todos de carpeta).
    pub fn system() -> Option<Self> {
        omni_store::Store::default_path().map(Self::for_db)
    }

    pub fn for_db(db: PathBuf) -> Self {
        let dir = db.parent().map(Path::to_path_buf).unwrap_or_default();
        Self {
            endpoint: dir.join("omnid.json"),
            lock: dir.join("omnid.lock"),
            log: dir.join("omnid.log"),
            db,
        }
    }

    pub fn dir(&self) -> &Path {
        self.db.parent().unwrap_or(Path::new("."))
    }
}

/// Contenido de `omnid.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub port: u16,
    pub token: String,
    pub pid: u32,
    pub version: String,
}

impl Endpoint {
    pub fn read(path: &Path) -> Option<Self> {
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    }

    /// Escribe el archivo de forma atómica y, en Unix, legible solo por el
    /// usuario (el token es la llave del servicio).
    pub fn write(&self, path: &Path) -> io::Result<()> {
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(self)?;
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(&data)?;
        }
        #[cfg(not(unix))]
        std::fs::write(&tmp, &data)?;
        std::fs::rename(&tmp, path)
    }
}
