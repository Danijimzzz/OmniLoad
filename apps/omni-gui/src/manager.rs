//! Cola de descargas de la ventana.
//!
//! No sabe nada de Slint: guarda las descargas agrupadas en paquetes, decide
//! cuáles arrancar (prioridad y límite de descargas simultáneas), las lanza en
//! el runtime de tokio y devuelve una foto ([`RowInfo`]) que la interfaz pinta.
//! Mientras no exista el demonio `omnid` (fase 2), el motor corre en el mismo
//! proceso que la ventana.
//!
//! Pausar es cancelar: el motor guarda el progreso en `<archivo>.part.omni` y,
//! al reanudar, una descarga nueva con la misma URL y carpeta sigue donde se
//! quedó.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    /// Esperando turno en la cola.
    Queued,
    /// Lanzada, aún sondeando el servidor.
    Starting,
    Running,
    /// Se pidió pausar y el motor está guardando el progreso.
    Pausing,
    /// Parada: no arranca hasta que se pida.
    Paused,
    Done,
    Failed,
}

impl State {
    pub fn is_active(self) -> bool {
        matches!(self, State::Starting | State::Running | State::Pausing)
    }
}

/// Prioridad en la cola: las altas arrancan antes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    High,
    #[default]
    Normal,
    Low,
}

impl Priority {
    pub const ALL: [Priority; 3] = [Priority::High, Priority::Normal, Priority::Low];

    pub fn label(self) -> &'static str {
        match self {
            Priority::High => "Alta",
            Priority::Normal => "Normal",
            Priority::Low => "Baja",
        }
    }

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn from_index(i: usize) -> Self {
        Self::ALL.get(i).copied().unwrap_or_default()
    }
}

/// Una descarga nueva para la cola.
#[derive(Clone, Debug)]
pub struct NewDownload {
    pub url: String,
    pub dir: PathBuf,
    /// Nombre provisional (el real lo confirma el motor).
    pub name: Option<String>,
    pub package: String,
    pub priority: Priority,
    /// Tamaño conocido de antemano (del capturador), hasta que el motor lo confirme.
    pub size: Option<u64>,
    /// `true`: a la cola; `false`: se añade parada.
    pub start: bool,
}

/// Lo que la interfaz necesita para pintar una descarga.
#[derive(Clone, Debug, PartialEq)]
pub struct RowInfo {
    pub id: u64,
    pub package: String,
    pub name: String,
    pub host: String,
    pub folder: PathBuf,
    pub url: String,
    pub state: State,
    pub priority: Priority,
    /// La cola está en pausa global (para decir "En pausa" en vez de "En
    /// espera").
    pub held: bool,
    pub downloaded: u64,
    pub total: Option<u64>,
    /// Bytes por segundo (0 si no está descargando).
    pub rate: f64,
    pub receiving: usize,
    pub connections: usize,
    /// Conexiones que acepta el servidor (0 = sin límite conocido).
    pub server_limit: usize,
    pub retries: u64,
    pub started_once: bool,
    pub detail: String,
}

impl RowInfo {
    pub fn fraction(&self) -> Option<f32> {
        if self.state == State::Done {
            return Some(1.0);
        }
        match self.total {
            Some(0) => Some(1.0),
            Some(t) => Some((self.downloaded as f64 / t as f64).clamp(0.0, 1.0) as f32),
            None => None,
        }
    }

    /// Segundos que faltan, si se puede estimar.
    pub fn eta(&self) -> Option<Duration> {
        let t = self.total?;
        (self.rate > 0.0 && t > self.downloaded)
            .then(|| Duration::from_secs_f64((t - self.downloaded) as f64 / self.rate))
    }

    /// Texto corto del estado, como en la columna "Estado".
    pub fn status(&self) -> String {
        match self.state {
            State::Queued if self.held => "En pausa".into(),
            State::Queued => "En espera".into(),
            State::Starting => "Conectando…".into(),
            State::Running => {
                let mut s = format!("Descargando · {} conex.", self.receiving);
                if self.server_limit > 0 && self.server_limit < self.connections {
                    s.push_str(&format!(" (máx. {})", self.server_limit));
                }
                s
            }
            State::Pausing => "Pausando…".into(),
            State::Paused if self.started_once => "Pausada".into(),
            State::Paused => "Sin iniciar".into(),
            State::Done => "Completada".into(),
            State::Failed => match self.detail.as_str() {
                d if d.contains("caducado") => "Enlace caducado".into(),
                _ => "Error".into(),
            },
        }
    }
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
    id: u64,
    url: String,
    dir: PathBuf,
    /// Nombre provisional (sale de la URL o del capturador); se cambia por el
    /// real al terminar.
    name: String,
    package: String,
    priority: Priority,
    state: State,
    /// Al terminar de pausar, ¿vuelve a la cola (pausa global) o se queda
    /// parada?
    requeue_after_pause: bool,
    started_once: bool,
    connections: usize,
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

/// La cola de descargas.
pub struct Manager {
    rt: Handle,
    base: DownloadOptions,
    entries: Arc<Mutex<Vec<Entry>>>,
    next_id: Mutex<u64>,
    max_active: AtomicUsize,
    connections: AtomicUsize,
    /// Pausa global: no arranca nada de la cola.
    held: AtomicBool,
}

impl Manager {
    /// `base` son las opciones de cada descarga (salvo las conexiones, que
    /// elige el usuario).
    pub fn new(rt: Handle, base: DownloadOptions) -> Self {
        Self {
            rt,
            connections: AtomicUsize::new(base.connections_per_network),
            base,
            entries: Arc::default(),
            next_id: Mutex::new(0),
            max_active: AtomicUsize::new(3),
            held: AtomicBool::new(false),
        }
    }

    fn entries(&self) -> MutexGuard<'_, Vec<Entry>> {
        self.entries.lock().expect("lista de descargas envenenada")
    }

    fn next_id(&self) -> u64 {
        let mut n = self.next_id.lock().expect("contador envenenado");
        *n += 1;
        *n
    }

    /// Descargas a la vez (las demás esperan en la cola).
    pub fn set_max_active(&self, n: usize) {
        self.max_active.store(n.max(1), Ordering::Relaxed);
        self.pump();
    }

    /// Conexiones por archivo para las descargas que arranquen a partir de
    /// ahora.
    pub fn set_connections(&self, n: usize) {
        self.connections.store(n.max(1), Ordering::Relaxed);
    }

    /// Añade una descarga. Si ya está en la lista (misma URL y carpeta) y
    /// estaba parada, la vuelve a poner en marcha en vez de duplicarla.
    pub fn add(&self, new: NewDownload) -> Result<u64, String> {
        let url = new.url.trim().to_owned();
        if url.is_empty() {
            return Err("Pega un enlace primero.".into());
        }
        if new.dir.as_os_str().is_empty() {
            return Err("Elige una carpeta de destino.".into());
        }
        // Validamos ya la URL para avisar sin añadir una fila que va a fallar.
        Download::new(&url, Destination::Dir(new.dir.clone()), self.base.clone())
            .map_err(|e| e.to_string())?;

        {
            let mut entries = self.entries();
            if let Some(e) = entries
                .iter_mut()
                .find(|e| e.url == url && e.dir == new.dir)
            {
                return match e.state {
                    State::Paused | State::Failed => {
                        if new.start {
                            e.state = State::Queued;
                        }
                        let id = e.id;
                        drop(entries);
                        self.pump();
                        Ok(id)
                    }
                    State::Done => Err("ya está descargado en esa carpeta".into()),
                    _ => Err("ya está en la cola".into()),
                };
            }
        }

        let name = new.name.unwrap_or_else(|| {
            url::Url::parse(&url)
                .ok()
                .and_then(|u| omni_core::filename::from_url(&u))
                .unwrap_or_else(|| url.clone())
        });
        let id = self.next_id();
        self.entries().push(Entry {
            id,
            url,
            dir: new.dir,
            name,
            package: new.package,
            priority: new.priority,
            state: if new.start {
                State::Queued
            } else {
                State::Paused
            },
            requeue_after_pause: false,
            started_once: false,
            connections: self.connections.load(Ordering::Relaxed),
            run_id: 0,
            progress: None,
            cancel: None,
            task: None,
            speed: SpeedMeter::default(),
            detail: String::new(),
            last_total: new.size,
            last_downloaded: 0,
        });
        self.pump();
        Ok(id)
    }

    /// Arranca lo que toque de la cola: primero la prioridad más alta y, a
    /// igual prioridad, lo que se añadió antes.
    pub fn pump(&self) {
        if self.held.load(Ordering::Relaxed) {
            return;
        }
        let max = self.max_active.load(Ordering::Relaxed);
        loop {
            let next = {
                let entries = self.entries();
                let active = entries.iter().filter(|e| e.state.is_active()).count();
                if active >= max {
                    return;
                }
                entries
                    .iter()
                    .filter(|e| e.state == State::Queued)
                    .min_by_key(|e| (e.priority, e.id))
                    .map(|e| e.id)
            };
            match next {
                Some(id) => self.launch(id),
                None => return,
            }
        }
    }

    /// Lanza (o relanza) la descarga `id`.
    fn launch(&self, id: u64) {
        let run_id = self.next_id();
        let connections = self.connections.load(Ordering::Relaxed);
        let mut entries = self.entries();
        let Some(e) = entries.iter_mut().find(|e| e.id == id) else {
            return;
        };
        e.connections = connections;
        let opts = DownloadOptions {
            connections_per_network: connections,
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
        e.started_once = true;
        e.requeue_after_pause = false;
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
                Err(Error::Cancelled) => {
                    e.state = if e.requeue_after_pause {
                        State::Queued
                    } else {
                        State::Paused
                    };
                }
                Err(err) => {
                    warn!(url = %e.url, "descarga fallida: {err}");
                    e.state = State::Failed;
                    e.detail = err.to_string();
                }
            }
        }));
    }

    fn cancel_entry(e: &mut Entry, requeue: bool) {
        match e.state {
            State::Starting | State::Running => {
                if let Some(c) = &e.cancel {
                    c.cancel();
                }
                e.state = State::Pausing;
                e.requeue_after_pause = requeue;
            }
            State::Pausing => e.requeue_after_pause = requeue,
            State::Queued if !requeue => e.state = State::Paused,
            _ => {}
        }
    }

    /// Pone en la cola las descargas paradas o fallidas de `ids`.
    pub fn start(&self, ids: &[u64]) {
        for e in self.entries().iter_mut().filter(|e| ids.contains(&e.id)) {
            if matches!(e.state, State::Paused | State::Failed) {
                e.state = State::Queued;
            }
            if e.state == State::Pausing {
                e.requeue_after_pause = true;
            }
        }
        self.pump();
    }

    /// Para las descargas de `ids` (las que corren se pausan guardando el
    /// progreso; las que esperaban se quedan paradas).
    pub fn stop(&self, ids: &[u64]) {
        for e in self.entries().iter_mut().filter(|e| ids.contains(&e.id)) {
            Self::cancel_entry(e, false);
        }
        self.pump();
    }

    /// Botón "Iniciar": quita la pausa global y pone en la cola todo lo
    /// parado.
    pub fn start_all(&self) {
        self.held.store(false, Ordering::Relaxed);
        let ids = self.ids();
        self.start(&ids);
    }

    /// Botón "Pausar": pausa lo que corre y no arranca nada más; al pulsar
    /// "Iniciar" todo sigue donde estaba.
    pub fn pause_all(&self) {
        self.held.store(true, Ordering::Relaxed);
        for e in self.entries().iter_mut() {
            Self::cancel_entry(e, true);
        }
    }

    /// Botón "Detener": para todo y vacía la cola.
    pub fn stop_all(&self) {
        self.held.store(false, Ordering::Relaxed);
        for e in self.entries().iter_mut() {
            Self::cancel_entry(e, false);
        }
    }

    pub fn is_held(&self) -> bool {
        self.held.load(Ordering::Relaxed)
    }

    fn ids(&self) -> Vec<u64> {
        self.entries().iter().map(|e| e.id).collect()
    }

    /// Quita descargas de la lista (las que corren se pausan antes). No borra
    /// nada del disco: volver a añadir el enlace reanuda desde el `.part`.
    pub fn remove(&self, ids: &[u64]) {
        self.entries().retain(|e| {
            let keep = !ids.contains(&e.id);
            if !keep && let Some(c) = &e.cancel {
                c.cancel();
            }
            keep
        });
        self.pump();
    }

    pub fn set_priority(&self, ids: &[u64], priority: Priority) {
        for e in self.entries().iter_mut().filter(|e| ids.contains(&e.id)) {
            e.priority = priority;
        }
    }

    /// Ids de las descargas de un paquete.
    pub fn package_ids(&self, package: &str) -> Vec<u64> {
        self.entries()
            .iter()
            .filter(|e| e.package == package)
            .map(|e| e.id)
            .collect()
    }

    /// ¿Hay alguna descarga en marcha?
    pub fn any_active(&self) -> bool {
        self.entries().iter().any(|e| e.state.is_active())
    }

    /// Foto de la lista para pintarla. Se llama unas cuatro veces por
    /// segundo; de paso arranca lo que toque de la cola.
    pub fn snapshot(&self) -> Vec<RowInfo> {
        self.pump();
        let now = Instant::now();
        let held = self.is_held();
        let mut entries = self.entries();
        entries.iter_mut().map(|e| row(e, now, held)).collect()
    }

    /// Pausa todo y espera (como mucho `timeout`) a que el motor guarde el
    /// progreso. Se llama al cerrar la ventana.
    pub fn shutdown(&self, timeout: Duration) {
        self.held.store(true, Ordering::Relaxed);
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

fn row(e: &mut Entry, now: Instant, held: bool) -> RowInfo {
    let mut receiving = 0;
    let mut server_limit = 0;
    let mut retries = 0;
    let (downloaded, total) = match &e.progress {
        Some(p) if e.state.is_active() => {
            let stats = p.per_network();
            receiving = stats.values().map(|s| s.receiving).sum();
            server_limit = stats.values().map(|s| s.limit).sum();
            retries = stats.values().map(|s| s.retries).sum();
            let total = p.total().or(e.last_total);
            let downloaded = p.downloaded();
            e.last_total = total;
            e.last_downloaded = downloaded;
            (downloaded, total)
        }
        _ => (e.last_downloaded, e.last_total),
    };

    // En cuanto llegan datos o se conoce el tamaño, deja de "empezar".
    if e.state == State::Starting && (total.is_some() || downloaded > 0) {
        e.state = State::Running;
    }

    let rate = match (&e.progress, e.state) {
        (Some(p), State::Starting | State::Running) => {
            e.speed.sample(now, downloaded, p.resumed_from())
        }
        _ => 0.0,
    };

    RowInfo {
        id: e.id,
        package: e.package.clone(),
        name: e.name.clone(),
        host: url::Url::parse(&e.url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_default(),
        folder: e.dir.clone(),
        url: e.url.clone(),
        state: e.state,
        priority: e.priority,
        held,
        downloaded,
        total,
        rate,
        receiving,
        connections: e.connections,
        server_limit,
        retries,
        started_once: e.started_once,
        detail: e.detail.clone(),
    }
}

/// Texto "a de b" para el tamaño.
pub fn sizes(r: &RowInfo) -> String {
    match r.total {
        Some(t) => format::bytes(t),
        None if r.downloaded > 0 => format::bytes(r.downloaded),
        None => String::new(),
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

    fn new(url: String, dir: &std::path::Path, start: bool) -> NewDownload {
        NewDownload {
            url,
            dir: dir.into(),
            name: None,
            package: "Pruebas".into(),
            priority: Priority::Normal,
            size: None,
            start,
        }
    }

    async fn wait_for(m: &Manager, id: u64, want: State) -> RowInfo {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let row = m.snapshot().into_iter().find(|r| r.id == id).unwrap();
            if row.state == want {
                return row;
            }
            assert!(
                Instant::now() < deadline,
                "la descarga no llegó a {want:?}: {row:?}"
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

        let id = m
            .add(new(server.url("video.bin"), dir.path(), true))
            .unwrap();
        assert_eq!(m.snapshot()[0].name, "video.bin");
        let row = wait_for(&m, id, State::Running).await;
        assert!(row.fraction().is_some());

        while m.snapshot()[0].fraction().unwrap_or(0.0) < 0.1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        m.stop(&[id]);
        let paused = wait_for(&m, id, State::Paused).await;
        assert_eq!(paused.status(), "Pausada");
        assert!(!dir.path().join("video.bin").exists());

        // Añadir el mismo enlace otra vez lo reanuda en lugar de duplicarlo.
        server.set_chunk_delay(None);
        assert_eq!(
            m.add(new(server.url("video.bin"), dir.path(), true)),
            Ok(id)
        );
        assert_eq!(m.snapshot().len(), 1);
        let done = wait_for(&m, id, State::Done).await;
        assert_eq!(done.fraction(), Some(1.0));
        assert_eq!(std::fs::read(dir.path().join("video.bin")).unwrap(), data);
        assert!(
            m.add(new(server.url("video.bin"), dir.path(), true))
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queue_respects_limit_and_priority() {
        let cfg = ServerConfig {
            chunk: 16 * 1024,
            chunk_delay: Some(Duration::from_millis(20)),
            ..ServerConfig::default()
        };
        let server = TestServer::start(pseudo_random_bytes(1024 * 1024, 3), cfg).await;
        let dir = tempfile::tempdir().unwrap();
        let m = Manager::new(Handle::current(), fast_opts());
        m.set_max_active(1);

        // Añadida parada: no arranca sola.
        let parked = m
            .add(new(server.url("parada.bin"), dir.path(), false))
            .unwrap();
        let a = m.add(new(server.url("a.bin"), dir.path(), true)).unwrap();
        let b = m.add(new(server.url("b.bin"), dir.path(), true)).unwrap();
        let mut urgent = new(server.url("urgente.bin"), dir.path(), true);
        urgent.priority = Priority::High;
        let c = m.add(urgent).unwrap();

        let rows = m.snapshot();
        let state = |id| rows.iter().find(|r| r.id == id).unwrap().state;
        assert!(state(a).is_active());
        assert_eq!(state(b), State::Queued);
        assert_eq!(state(c), State::Queued);
        assert_eq!(state(parked), State::Paused);
        assert_eq!(
            rows.iter().find(|r| r.id == parked).unwrap().status(),
            "Sin iniciar"
        );

        // Al parar la primera, entra la de prioridad alta antes que b.
        m.stop(&[a]);
        wait_for(&m, a, State::Paused).await;
        let rows = m.snapshot();
        let state = |id| rows.iter().find(|r| r.id == id).unwrap().state;
        assert!(state(c).is_active(), "{rows:?}");
        assert_eq!(state(b), State::Queued);

        // Pausa global: lo que corre vuelve a la cola y no arranca nada.
        m.pause_all();
        wait_for(&m, c, State::Queued).await;
        assert!(!m.any_active());
        assert_eq!(
            m.snapshot().iter().find(|r| r.id == b).unwrap().status(),
            "En pausa"
        );
        m.stop_all();
        assert!(m.snapshot().iter().all(|r| r.state == State::Paused));
        m.remove(&[a, b, c, parked]);
        assert!(m.snapshot().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalid_url_is_rejected_without_row() {
        let m = Manager::new(Handle::current(), fast_opts());
        let dir = std::path::Path::new(".");
        assert!(m.add(new("ftp://ejemplo.com/a".into(), dir, true)).is_err());
        assert!(m.add(new("   ".into(), dir, true)).is_err());
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
