//! Almacén de OmniLoad en SQLite.
//!
//! Guarda lo que tiene que sobrevivir a un cierre del programa:
//!
//! - la lista de descargas (cola, pausadas, terminadas…);
//! - los ajustes de la cola (descargas a la vez, límites…);
//! - cuántas conexiones acepta cada servidor, para no tener que volver a
//!   averiguarlo a base de rechazos.
//!
//! El progreso fino de cada archivo (qué bloques faltan) sigue en el
//! `.part.omni` junto al archivo, que el motor escribe con `fsync` cada medio
//! segundo: así una descarga se puede reanudar aunque se borre la base de
//! datos. Aquí solo se guarda un resumen (bytes descargados) para pintarlo.
//!
//! La base de datos está en la carpeta de datos del sistema
//! (`%APPDATA%\OmniLoad` en Windows, `~/.local/share/omniload` en Linux,
//! `~/Library/Application Support/OmniLoad` en macOS). La variable de entorno
//! `OMNILOAD_DB` la cambia de sitio (útil en pruebas).

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use omni_core::ServerLimit;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Errores del almacén.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("error de la base de datos: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("dato ilegible en la base de datos: {0}")]
    Json(#[from] serde_json::Error),
    #[error("no se pudo crear la carpeta de la base de datos: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Una descarga tal como se guarda.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JobRecord {
    /// 0 al insertar: lo asigna la base de datos.
    pub id: u64,
    /// Enlace tal como lo añadió el usuario. En la fase de plugins, el
    /// enlace directo se resuelve en cada arranque (los de Mega, Gofile,
    /// Drive o Mediafire caducan), así que se guarda el original.
    pub url: String,
    pub dir: PathBuf,
    pub name: String,
    pub package: String,
    /// 0 alta, 1 normal, 2 baja.
    pub priority: u8,
    /// Estado ("queued", "paused", "done"…); lo interpreta la cola.
    pub state: String,
    pub started_once: bool,
    pub connections: u32,
    /// Límite de velocidad propio, en bytes por segundo.
    pub speed_limit: Option<u64>,
    pub total: Option<u64>,
    pub downloaded: u64,
    pub detail: String,
    /// Redes por las que descargar (nombres); vacío = la del sistema.
    pub networks: Vec<String>,
    /// Segundos desde 1970.
    pub created_at: u64,
    pub finished_at: Option<u64>,
}

/// Migraciones, en orden. La versión de la base de datos es
/// `PRAGMA user_version`; nunca se cambia una migración ya publicada, solo se
/// añaden nuevas al final.
const MIGRATIONS: &[&str] = &[
    // 1: primera versión.
    "CREATE TABLE jobs (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        url          TEXT    NOT NULL,
        dir          TEXT    NOT NULL,
        name         TEXT    NOT NULL,
        package      TEXT    NOT NULL,
        priority     INTEGER NOT NULL,
        state        TEXT    NOT NULL,
        started_once INTEGER NOT NULL,
        connections  INTEGER NOT NULL,
        speed_limit  INTEGER,
        total        INTEGER,
        downloaded   INTEGER NOT NULL,
        detail       TEXT    NOT NULL,
        networks     TEXT    NOT NULL,
        created_at   INTEGER NOT NULL,
        finished_at  INTEGER
    );
    CREATE TABLE settings (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    CREATE TABLE server_limits (
        host       TEXT    NOT NULL,
        network    TEXT    NOT NULL,
        max        INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        PRIMARY KEY (host, network)
    );",
];

/// La base de datos. Se puede compartir entre hilos.
pub struct Store {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

/// Segundos desde 1970.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Store {
    /// Ruta por defecto de la base de datos (ver la documentación del
    /// módulo).
    pub fn default_path() -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("OMNILOAD_DB") {
            return Some(PathBuf::from(p));
        }
        let base = dirs::data_dir()?;
        let folder = if cfg!(any(target_os = "windows", target_os = "macos")) {
            "OmniLoad"
        } else {
            "omniload"
        };
        Some(base.join(folder).join("omniload.db"))
    }

    /// Abre (o crea) la base de datos en `path`.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        // WAL: escrituras rápidas y seguras ante cortes; NORMAL basta con WAL.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(Duration::from_secs(5))?;
        Self::init(conn)
    }

    /// Base de datos en memoria (pruebas).
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let version = version.max(0) as usize;
        if version < MIGRATIONS.len() {
            let tx = conn.transaction()?;
            for sql in &MIGRATIONS[version..] {
                tx.execute_batch(sql)?;
            }
            tx.pragma_update(None, "user_version", MIGRATIONS.len() as i64)?;
            tx.commit()?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().expect("base de datos envenenada")
    }

    // ---- Descargas ----

    /// Inserta una descarga y devuelve su id.
    pub fn insert_job(&self, job: &JobRecord) -> Result<u64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO jobs (url, dir, name, package, priority, state, started_once,
                connections, speed_limit, total, downloaded, detail, networks, created_at,
                finished_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                job.url,
                path_str(&job.dir),
                job.name,
                job.package,
                job.priority,
                job.state,
                job.started_once,
                job.connections,
                job.speed_limit.map(to_i64),
                job.total.map(to_i64),
                to_i64(job.downloaded),
                job.detail,
                serde_json::to_string(&job.networks)?,
                to_i64(job.created_at),
                job.finished_at.map(to_i64),
            ],
        )?;
        Ok(conn.last_insert_rowid() as u64)
    }

    /// Guarda todos los campos de una descarga ya insertada.
    pub fn update_job(&self, job: &JobRecord) -> Result<()> {
        self.conn().execute(
            "UPDATE jobs SET url = ?2, dir = ?3, name = ?4, package = ?5, priority = ?6,
                state = ?7, started_once = ?8, connections = ?9, speed_limit = ?10,
                total = ?11, downloaded = ?12, detail = ?13, networks = ?14,
                finished_at = ?15
             WHERE id = ?1",
            params![
                to_i64(job.id),
                job.url,
                path_str(&job.dir),
                job.name,
                job.package,
                job.priority,
                job.state,
                job.started_once,
                job.connections,
                job.speed_limit.map(to_i64),
                job.total.map(to_i64),
                to_i64(job.downloaded),
                job.detail,
                serde_json::to_string(&job.networks)?,
                job.finished_at.map(to_i64),
            ],
        )?;
        Ok(())
    }

    /// Guarda solo el progreso de varias descargas, en una transacción.
    pub fn update_progress(&self, rows: &[(u64, u64, Option<u64>)]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        {
            let mut stmt =
                tx.prepare_cached("UPDATE jobs SET downloaded = ?2, total = ?3 WHERE id = ?1")?;
            for &(id, downloaded, total) in rows {
                stmt.execute(params![to_i64(id), to_i64(downloaded), total.map(to_i64)])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Quita descargas de la lista.
    pub fn delete_jobs(&self, ids: &[u64]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached("DELETE FROM jobs WHERE id = ?1")?;
            for &id in ids {
                stmt.execute(params![to_i64(id)])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Todas las descargas, en el orden en que se añadieron.
    pub fn jobs(&self) -> Result<Vec<JobRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, url, dir, name, package, priority, state, started_once, connections,
                speed_limit, total, downloaded, detail, networks, created_at, finished_at
             FROM jobs ORDER BY id",
        )?;
        let rows = stmt.query_map([], read_job)?;
        let mut out = Vec::new();
        for r in rows {
            let (mut job, networks) = r?;
            job.networks = serde_json::from_str(&networks).unwrap_or_default();
            out.push(job);
        }
        Ok(out)
    }

    // ---- Ajustes ----

    /// Lee un ajuste guardado como JSON. `None` si no existe o no se puede
    /// leer como `T` (por ejemplo, de una versión antigua).
    pub fn setting<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let value: Option<String> = self
            .conn()
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(value.and_then(|v| serde_json::from_str(&v).ok()))
    }

    pub fn set_setting<T: Serialize + ?Sized>(&self, key: &str, value: &T) -> Result<()> {
        self.conn().execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, serde_json::to_string(value)?],
        )?;
        Ok(())
    }

    // ---- Servidores ----

    /// Límites de conexiones aprendidos hace menos de `max_age` (los
    /// servidores cambian: lo de hace meses ya no vale).
    pub fn server_limits(&self, max_age: Duration) -> Result<Vec<ServerLimit>> {
        let since = now_secs().saturating_sub(max_age.as_secs());
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT host, network, max FROM server_limits WHERE updated_at >= ?1
             ORDER BY host, network",
        )?;
        let rows = stmt.query_map([to_i64(since)], |r| {
            Ok(ServerLimit {
                host: r.get(0)?,
                network: r.get(1)?,
                max: r.get::<_, i64>(2)?.max(1) as usize,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Guarda límites aprendidos (los que no cambian conservan su fecha).
    pub fn save_server_limits(&self, limits: &[ServerLimit]) -> Result<()> {
        if limits.is_empty() {
            return Ok(());
        }
        let now = to_i64(now_secs());
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO server_limits (host, network, max, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(host, network) DO UPDATE SET
                    updated_at = CASE WHEN max = excluded.max THEN updated_at
                                      ELSE excluded.updated_at END,
                    max = excluded.max",
            )?;
            for l in limits {
                stmt.execute(params![l.host, l.network, l.max as i64, now])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

fn read_job(r: &Row<'_>) -> rusqlite::Result<(JobRecord, String)> {
    Ok((
        JobRecord {
            id: r.get::<_, i64>(0)? as u64,
            url: r.get(1)?,
            dir: PathBuf::from(r.get::<_, String>(2)?),
            name: r.get(3)?,
            package: r.get(4)?,
            priority: r.get(5)?,
            state: r.get(6)?,
            started_once: r.get(7)?,
            connections: r.get(8)?,
            speed_limit: r.get::<_, Option<i64>>(9)?.map(|v| v as u64),
            total: r.get::<_, Option<i64>>(10)?.map(|v| v as u64),
            downloaded: r.get::<_, i64>(11)? as u64,
            detail: r.get(12)?,
            networks: Vec::new(),
            created_at: r.get::<_, i64>(14)? as u64,
            finished_at: r.get::<_, Option<i64>>(15)?.map(|v| v as u64),
        },
        r.get(13)?,
    ))
}

/// SQLite guarda enteros con signo de 64 bits; los tamaños nunca llegan a
/// 2^63.
fn to_i64(v: u64) -> i64 {
    v.min(i64::MAX as u64) as i64
}

/// Las rutas se guardan como texto. En Windows y macOS siempre son UTF-8
/// válido en la práctica; en Linux una ruta con bytes raros se guarda con
/// sustituciones (y la descarga iría a una carpeta parecida).
fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(url: &str) -> JobRecord {
        JobRecord {
            url: url.into(),
            dir: PathBuf::from("/descargas/Paquete"),
            name: "archivo.zip".into(),
            package: "Paquete".into(),
            priority: 1,
            state: "queued".into(),
            connections: 8,
            networks: vec!["sistema".into()],
            created_at: now_secs(),
            ..JobRecord::default()
        }
    }

    #[test]
    fn jobs_roundtrip() {
        let s = Store::open_in_memory().unwrap();
        let mut a = job("https://ejemplo.com/a.zip");
        a.id = s.insert_job(&a).unwrap();
        let mut b = job("https://ejemplo.com/b.zip");
        b.speed_limit = Some(1_000_000);
        b.id = s.insert_job(&b).unwrap();
        assert_eq!(s.jobs().unwrap(), vec![a.clone(), b.clone()]);

        a.state = "done".into();
        a.total = Some(1234);
        a.downloaded = 1234;
        a.finished_at = Some(now_secs());
        s.update_job(&a).unwrap();
        s.update_progress(&[(b.id, 99, Some(500))]).unwrap();
        b.downloaded = 99;
        b.total = Some(500);
        assert_eq!(s.jobs().unwrap(), vec![a.clone(), b.clone()]);

        s.delete_jobs(&[a.id]).unwrap();
        assert_eq!(s.jobs().unwrap(), vec![b]);
    }

    #[test]
    fn settings_roundtrip() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.setting::<u32>("max_active").unwrap(), None);
        s.set_setting("max_active", &3u32).unwrap();
        s.set_setting("max_active", &5u32).unwrap();
        assert_eq!(s.setting::<u32>("max_active").unwrap(), Some(5));
        // Un valor que ya no encaja con el tipo se ignora.
        assert_eq!(s.setting::<Vec<String>>("max_active").unwrap(), None);
    }

    #[test]
    fn server_limits_roundtrip() {
        let s = Store::open_in_memory().unwrap();
        let lim = ServerLimit {
            host: "lento.com".into(),
            network: "sistema".into(),
            max: 2,
        };
        s.save_server_limits(std::slice::from_ref(&lim)).unwrap();
        s.save_server_limits(&[ServerLimit {
            max: 3,
            ..lim.clone()
        }])
        .unwrap();
        let got = s.server_limits(Duration::from_secs(3600)).unwrap();
        assert_eq!(got, vec![ServerLimit { max: 3, ..lim }]);
    }

    #[test]
    fn survives_reopening_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("omniload.db");
        let id = {
            let s = Store::open(&path).unwrap();
            s.insert_job(&job("https://ejemplo.com/a.zip")).unwrap()
        };
        let s = Store::open(&path).unwrap();
        let jobs = s.jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, id);
    }
}
