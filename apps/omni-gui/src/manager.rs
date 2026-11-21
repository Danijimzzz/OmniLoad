//! Lista de descargas de la ventana.
//!
//! No sabe nada de Slint: guarda las descargas, las lanza en el runtime de
//! tokio y devuelve una foto ([`RowInfo`]) que la interfaz convierte en filas.
//! Mientras no exista el demonio `omnid` (fase 2), el motor corre en el mismo
//! proceso que la ventana.
//!
//! Pausar es cancelar: el motor guarda el progreso en `<archivo>.part.omni` y,
//! al reanudar, una descarga nueva con la misma URL y carpeta sigue donde se
//! quedó.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use omni_core::{Destination, Download, DownloadOptions, Error, Progress};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::format;

/// En qué punto está una descarga.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Lanzada, aún sondeando el servidor.
    Starting,
    Running,
    /// Se pidió pausar y el motor está guardando el progreso.
    Pausing,
    Paused,
    Done,
    Failed,
}

/// Lo que la interfaz necesita para pintar una fila.
#[derive(Clone, Debug, PartialEq)]
pub struct RowInfo {
    pub name: String,
    pub folder: String,
    pub state: State,
    /// 0..1, o `None` si no se conoce el tamaño.
    pub fraction: Option<f32>,
    pub status: String,
    pub sizes: String,
    pub speed: String,
    pub eta: String,
    pub connections: String,
    pub detail: String,
}

/// Velocidad suavizada a partir de muestras de bytes descargados.
#[derive(Debug, Default)]
struct SpeedMeter {
    last: Option<(Instant, u64)>,
    /// Bytes que había al reanudar en la última muestra: si cambia, el salto
    /// de bytes es lo que ya estaba en disco, no velocidad.
    resumed_from: u64,
    rate: f64,
}

impl SpeedMeter {
    fn sample(&mut self, now: Instant, downloaded: u64, resumed_from: u64) -> f64 {
        match self.last {
            Some((t, prev)) if resumed_from == self.resumed_from && downloaded >= prev => {
                let dt = now.duration_since(t).as_secs_f64();
                if dt >= 0.2 {
                    let inst = (downloaded - prev) as f64 / dt;
                    // Media exponencial: estable pero reacciona en ~2 s.
                    self.rate = if self.rate == 0.0 {
                        inst
                    } else {
                        0.7 * self.rate + 0.3 * inst
                    };
                    self.last = Some((now, downloaded));
                }
            }
            _ => {
                self.last = Some((now, downloaded));
                self.resumed_from = resumed_from;
            }
        }
        self.rate
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

struct Entry {
    url: String,
    dir: PathBuf,
    /// Nombre provisional (sale de la URL); se cambia por el real al terminar.
    name: String,
    connections: usize,
    state: State,
    /// Cada lanzamiento tiene su número, para que una tarea vieja que acaba
    /// tarde no pise el estado de la nueva.
    run_id: u64,
    progress: Option<Arc<Progress>>,
    cancel: Option<CancellationToken>,
    task: Option<JoinHandle<()>>,
    speed: SpeedMeter,
    detail: String,
    /// Último tamaño conocido, para seguir mostrándolo en pausa.
    last_total: Option<u64>,
    last_downloaded: u64,
}

/// Las descargas de la ventana.
pub struct Manager {
    rt: Handle,
    base: DownloadOptions,
    entries: Arc<Mutex<Vec<Entry>>>,
    next_run: Mutex<u64>,
}

impl Manager {
    /// `base` son las opciones de cada descarga (salvo las conexiones, que
    /// elige el usuario).
    pub fn new(rt: Handle, base: DownloadOptions) -> Self {
        Self {
            rt,
            base,
            entries: Arc::default(),
            next_run: Mutex::new(0),
        }
    }

    fn entries(&self) -> MutexGuard<'_, Vec<Entry>> {
        self.entries.lock().expect("lista de descargas envenenada")
    }

    /// Añade una descarga y la lanza. Si ya está en la lista (misma URL y
    /// carpeta) y estaba parada, la reanuda en vez de duplicarla. `name` es el
    /// nombre que mostrar mientras el motor no confirme el real (el que
    /// encontró el capturador, por ejemplo).
    pub fn add(
        &self,
        url: &str,
        dir: PathBuf,
        connections: usize,
        name: Option<String>,
    ) -> Result<(), String> {
        let url = url.trim();
        if url.is_empty() {
            return Err("Pega un enlace primero.".into());
        }
        if dir.as_os_str().is_empty() {
            return Err("Elige una carpeta de destino.".into());
        }
        // Validamos ya la URL para avisar sin añadir una fila que va a fallar.
        Download::new(url, Destination::Dir(dir.clone()), self.base.clone())
            .map_err(|e| e.to_string())?;

        let existing = self
            .entries()
            .iter()
            .position(|e| e.url == url && e.dir == dir);
        if let Some(i) = existing {
            let state = self.entries()[i].state;
            return match state {
                State::Paused | State::Failed => {
                    self.entries()[i].connections = connections.max(1);
                    self.resume(i);
                    Ok(())
                }
                State::Done => Err("Esa descarga ya está terminada en esa carpeta.".into()),
                _ => Err("Esa descarga ya está en marcha.".into()),
            };
        }

        let name = name.unwrap_or_else(|| {
            url::Url::parse(url)
                .ok()
                .and_then(|u| omni_core::filename::from_url(&u))
                .unwrap_or_else(|| url.to_owned())
        });
        let i = {
            let mut entries = self.entries();
            entries.push(Entry {
                url: url.to_owned(),
                dir,
                name,
                connections: connections.max(1),
                state: State::Paused,
                run_id: 0,
                progress: None,
                cancel: None,
                task: None,
                speed: SpeedMeter::default(),
                detail: String::new(),
                last_total: None,
                last_downloaded: 0,
            });
            entries.len() - 1
        };
        self.start(i);
        Ok(())
    }

    /// Lanza (o relanza) la descarga `i`.
    fn start(&self, i: usize) {
        let run_id = {
            let mut n = self.next_run.lock().expect("contador envenenado");
            *n += 1;
            *n
        };
        let mut entries = self.entries();
        let Some(e) = entries.get_mut(i) else { return };
        let opts = DownloadOptions {
            connections_per_network: e.connections,
            ..self.base.clone()
        };
        let download = match Download::new(&e.url, Destination::Dir(e.dir.clone()), opts) {
            Ok(d) => d,
            Err(err) => {
                e.state = State::Failed;
                e.detail = err.to_string();
                return;
            }
        };
        let cancel = CancellationToken::new();
        e.state = State::Starting;
        e.run_id = run_id;
        e.detail.clear();
        e.speed.reset();
        e.progress = Some(download.progress());
        e.cancel = Some(cancel.clone());
        info!(url = %e.url, "descarga lanzada");

        let entries_ref = Arc::clone(&self.entries);
        e.task = Some(self.rt.spawn(async move {
            let result = download.run(cancel).await;
            let mut entries = entries_ref.lock().expect("lista de descargas envenenada");
            let Some(e) = entries.iter_mut().find(|e| e.run_id == run_id) else {
                return; // la quitaron de la lista
            };
            if let Some(p) = &e.progress {
                e.last_downloaded = p.downloaded();
                e.last_total = p.total().or(e.last_total);
            }
            e.cancel = None;
            e.task = None;
            match result {
                Ok(out) => {
                    if let Some(name) = out.path.file_name() {
                        e.name = name.to_string_lossy().into_owned();
                    }
                    e.state = State::Done;
                    e.last_total = Some(out.size);
                    e.last_downloaded = out.size;
                    e.detail = match &out.sha256 {
                        Some(h) => format!("SHA-256 correcto: {h}"),
                        None if !out.segmented => {
                            "El servidor no acepta rangos: se usó una sola conexión.".into()
                        }
                        None => String::new(),
                    };
                }
                Err(Error::Cancelled) => e.state = State::Paused,
                Err(err) => {
                    warn!(url = %e.url, "descarga fallida: {err}");
                    e.state = State::Failed;
                    e.detail = err.to_string();
                }
            }
        }));
    }

    pub fn pause(&self, i: usize) {
        let mut entries = self.entries();
        if let Some(e) = entries.get_mut(i)
            && matches!(e.state, State::Starting | State::Running)
            && let Some(c) = &e.cancel
        {
            c.cancel();
            e.state = State::Pausing;
        }
    }

    pub fn resume(&self, i: usize) {
        let can = self
            .entries()
            .get(i)
            .is_some_and(|e| matches!(e.state, State::Paused | State::Failed));
        if can {
            self.start(i);
        }
    }

    /// Quita la descarga de la lista (si corre, la pausa antes). No borra
    /// nada del disco: volver a añadir el enlace reanuda desde el `.part`.
    pub fn remove(&self, i: usize) {
        let mut entries = self.entries();
        if i < entries.len() {
            let e = entries.remove(i);
            if let Some(c) = e.cancel {
                c.cancel();
            }
        }
    }

    /// Carpeta de la descarga `i`.
    pub fn folder(&self, i: usize) -> Option<PathBuf> {
        self.entries().get(i).map(|e| e.dir.clone())
    }

    /// ¿Hay alguna descarga en marcha?
    pub fn any_active(&self) -> bool {
        self.entries()
            .iter()
            .any(|e| matches!(e.state, State::Starting | State::Running | State::Pausing))
    }

    /// Foto de la lista para pintarla. Se llama unas cuatro veces por segundo.
    pub fn snapshot(&self) -> Vec<RowInfo> {
        let now = Instant::now();
        let mut entries = self.entries();
        entries.iter_mut().map(|e| row(e, now)).collect()
    }

    /// Pausa todo y espera (como mucho `timeout`) a que el motor guarde el
    /// progreso. Se llama al cerrar la ventana.
    pub fn shutdown(&self, timeout: Duration) {
        let tasks: Vec<_> = self
            .entries()
            .iter_mut()
            .filter_map(|e| {
                if let Some(c) = &e.cancel {
                    c.cancel();
                }
                e.task.take()
            })
            .collect();
        if tasks.is_empty() {
            return;
        }
        info!(
            "cerrando: guardando el progreso de {} descargas",
            tasks.len()
        );
        let _ = self
            .rt
            .block_on(async { tokio::time::timeout(timeout, join_all(tasks)).await });
    }
}

async fn join_all(tasks: Vec<JoinHandle<()>>) {
    for t in tasks {
        let _ = t.await;
    }
}

fn row(e: &mut Entry, now: Instant) -> RowInfo {
    let (downloaded, total, conn_stats) = match &e.progress {
        Some(p) if e.state != State::Done => {
            let stats = p.per_network();
            let total = p.total().or(e.last_total);
            let downloaded = p.downloaded();
            e.last_total = total;
            e.last_downloaded = downloaded;
            (downloaded, total, Some(stats))
        }
        _ => (e.last_downloaded, e.last_total, None),
    };

    // En cuanto llegan datos o se conoce el tamaño, deja de "empezar".
    if e.state == State::Starting && (total.is_some() || downloaded > 0) {
        e.state = State::Running;
    }

    let active = matches!(e.state, State::Starting | State::Running);
    let rate = match (&e.progress, active) {
        (Some(p), true) => e.speed.sample(now, downloaded, p.resumed_from()),
        _ => 0.0,
    };

    let fraction = match (total, e.state) {
        (_, State::Done) => Some(1.0),
        (Some(0), _) => Some(1.0),
        (Some(t), _) => Some((downloaded as f64 / t as f64).clamp(0.0, 1.0) as f32),
        (None, _) => None,
    };

    let status = match e.state {
        State::Starting => "Conectando…".to_owned(),
        State::Running => match fraction {
            Some(f) => format!("{:.0} %", f * 100.0),
            None => "Descargando".to_owned(),
        },
        State::Pausing => "Pausando…".to_owned(),
        State::Paused => match fraction {
            Some(f) => format!("En pausa ({:.0} %)", f * 100.0),
            None => "En pausa".to_owned(),
        },
        State::Done => "Terminada".to_owned(),
        State::Failed => "Error".to_owned(),
    };

    let sizes = match total {
        Some(t) if e.state == State::Done => format::bytes(t),
        Some(t) => format!("{} de {}", format::bytes(downloaded), format::bytes(t)),
        None if downloaded > 0 => format::bytes(downloaded),
        None => String::new(),
    };

    let (speed, eta) = if active && rate > 0.0 {
        let eta = total
            .filter(|&t| t > downloaded)
            .map(|t| {
                let secs = (t - downloaded) as f64 / rate;
                format!("quedan {}", format::duration(Duration::from_secs_f64(secs)))
            })
            .unwrap_or_default();
        (format::speed(rate), eta)
    } else {
        (String::new(), String::new())
    };

    let connections = match conn_stats {
        Some(stats) if active => {
            let receiving: usize = stats.values().map(|s| s.receiving).sum();
            let limit: usize = stats.values().map(|s| s.limit).sum();
            let retries: u64 = stats.values().map(|s| s.retries).sum();
            let mut s = format!("{receiving}/{} conexiones activas", e.connections);
            if limit > 0 && limit < e.connections {
                s.push_str(&format!(" · el servidor solo acepta {limit}"));
            }
            if retries > 0 {
                s.push_str(&format!(" · {retries} reintentos"));
            }
            s
        }
        _ => String::new(),
    };

    RowInfo {
        name: e.name.clone(),
        folder: e.dir.display().to_string(),
        state: e.state,
        fraction,
        status,
        sizes,
        speed,
        eta,
        connections,
        detail: e.detail.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omni_testserver::{ServerConfig, TestServer, pseudo_random_bytes};

    fn fast_opts() -> DownloadOptions {
        DownloadOptions {
            block_size: 64 * 1024,
            min_block_size: 16 * 1024,
            write_buffer: 16 * 1024,
            retry_base: Duration::from_millis(10),
            retry_max: Duration::from_millis(100),
            save_interval: Duration::from_millis(50),
            ..DownloadOptions::default()
        }
    }

    async fn wait_for(m: &Manager, i: usize, want: State) -> RowInfo {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let rows = m.snapshot();
            if rows[i].state == want {
                return rows[i].clone();
            }
            assert!(
                Instant::now() < deadline,
                "la descarga no llegó a {want:?}: {:?}",
                rows[i]
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn add_pause_resume_finish() {
        let data = pseudo_random_bytes(2 * 1024 * 1024, 7);
        let cfg = ServerConfig {
            chunk: 16 * 1024,
            chunk_delay: Some(Duration::from_millis(5)),
            ..ServerConfig::default()
        };
        let server = TestServer::start(data.clone(), cfg).await;
        let dir = tempfile::tempdir().unwrap();
        let m = Manager::new(Handle::current(), fast_opts());

        m.add(&server.url("video.bin"), dir.path().into(), 4, None)
            .unwrap();
        assert_eq!(m.snapshot()[0].name, "video.bin");
        let row = wait_for(&m, 0, State::Running).await;
        assert!(row.fraction.is_some());

        // Esperamos a que haya algo descargado y pausamos.
        while m.snapshot()[0].fraction.unwrap_or(0.0) < 0.1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        m.pause(0);
        let paused = wait_for(&m, 0, State::Paused).await;
        assert!(paused.status.starts_with("En pausa"), "{}", paused.status);
        assert!(!dir.path().join("video.bin").exists());

        // Añadir el mismo enlace otra vez reanuda en lugar de duplicar.
        server.set_chunk_delay(None);
        m.add(&server.url("video.bin"), dir.path().into(), 4, None)
            .unwrap();
        assert_eq!(m.snapshot().len(), 1);
        let done = wait_for(&m, 0, State::Done).await;
        assert_eq!(done.fraction, Some(1.0));
        assert_eq!(std::fs::read(dir.path().join("video.bin")).unwrap(), data);
        assert!(
            m.add(&server.url("video.bin"), dir.path().into(), 4, None)
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalid_url_is_rejected_without_row() {
        let m = Manager::new(Handle::current(), fast_opts());
        assert!(
            m.add("ftp://ejemplo.com/a", PathBuf::from("."), 4, None)
                .is_err()
        );
        assert!(m.add("   ", PathBuf::from("."), 4, None).is_err());
        assert!(m.snapshot().is_empty());
    }

    #[test]
    fn speed_ignores_resume_jump() {
        let mut s = SpeedMeter::default();
        let t0 = Instant::now();
        s.sample(t0, 0, 0);
        // Al reanudar, `downloaded` salta a lo que ya había en disco.
        let r = s.sample(t0 + Duration::from_secs(1), 500_000_000, 500_000_000);
        assert_eq!(r, 0.0);
        let r = s.sample(t0 + Duration::from_secs(2), 501_000_000, 500_000_000);
        assert!((r - 1_000_000.0).abs() < 1.0, "{r}");
    }
}
