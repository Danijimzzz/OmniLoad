//! La cola de descargas.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use omni_core::{
    Destination, Download, DownloadOptions, Error, Progress, RateLimit, ServerLimits, ServerStatus,
};
use omni_store::{JobRecord, Store, now_secs};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::resolver::{DirectLinks, Resolver};
use crate::types::{JobInfo, NewJob, Priority, QueueSettings, State};

/// Cada cuánto se revisa la cola (arrancar lo que toque, medir velocidad).
const TICK: Duration = Duration::from_millis(250);
/// Cada cuántas vueltas se guarda el progreso en la base de datos.
const SAVE_EVERY: u32 = 8;
/// Lo aprendido de un servidor vale durante un mes.
const SERVER_LIMITS_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);
/// Clave de los ajustes de la cola en la base de datos.
const SETTINGS_KEY: &str = "queue";

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
    /// Al terminar de pausar, ¿vuelve a la cola (pausa global o cierre) o se
    /// queda parada?
    requeue_after_pause: bool,
    started_once: bool,
    connections: usize,
    /// Límite de velocidad propio (se puede cambiar en marcha).
    rate: Arc<RateLimit>,
    /// Redes por las que descargar; vacío = la del sistema (fase 7).
    networks: Vec<String>,
    created_at: u64,
    finished_at: Option<u64>,
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

impl Entry {
    fn record(&self) -> JobRecord {
        JobRecord {
            id: self.id,
            url: self.url.clone(),
            dir: self.dir.clone(),
            name: self.name.clone(),
            package: self.package.clone(),
            priority: self.priority.index() as u8,
            state: self.state.as_str().to_owned(),
            started_once: self.started_once,
            connections: self.connections as u32,
            speed_limit: self.rate.get(),
            total: self.last_total,
            downloaded: self.last_downloaded,
            detail: self.detail.clone(),
            networks: self.networks.clone(),
            created_at: self.created_at,
            finished_at: self.finished_at,
        }
    }

    fn from_record(r: JobRecord) -> Self {
        // Lo que estaba en marcha al cerrar vuelve a la cola para seguir
        // solo; lo que se estaba pausando se queda pausado.
        let state = match State::parse(&r.state).unwrap_or(State::Paused) {
            State::Starting | State::Running => State::Queued,
            State::Pausing => State::Paused,
            s => s,
        };
        Self {
            id: r.id,
            url: r.url,
            dir: r.dir,
            name: r.name,
            package: r.package,
            priority: Priority::from_index(r.priority as usize),
            state,
            requeue_after_pause: false,
            started_once: r.started_once,
            connections: r.connections.max(1) as usize,
            rate: Arc::new(RateLimit::new(r.speed_limit)),
            networks: r.networks,
            created_at: r.created_at,
            finished_at: r.finished_at,
            run_id: 0,
            progress: None,
            cancel: None,
            task: None,
            speed: SpeedMeter::default(),
            detail: r.detail,
            last_total: r.total,
            last_downloaded: r.downloaded,
        }
    }
}

struct Inner {
    rt: Handle,
    base: DownloadOptions,
    store: Arc<Store>,
    resolver: Arc<dyn Resolver>,
    entries: Mutex<Vec<Entry>>,
    settings: Mutex<QueueSettings>,
    global_rate: Arc<RateLimit>,
    servers: Arc<ServerLimits>,
    next_run: AtomicU64,
    /// Cerrando: no arranca nada más y lo que corre vuelve a la cola.
    closing: AtomicBool,
    /// Fuera del horario de descargas: como una pausa, pero no es la del
    /// usuario.
    sched_blocked: AtomicBool,
    stop_ticker: CancellationToken,
}

/// La cola de descargas. Clonarla es barato: todas las copias son la misma
/// cola.
#[derive(Clone)]
pub struct Queue {
    inner: Arc<Inner>,
}

/// Opciones para abrir la cola.
pub struct QueueConfig {
    /// Opciones de cada descarga (las conexiones, los límites y las cabeceras
    /// los pone la cola).
    pub base: DownloadOptions,
    pub store: Arc<Store>,
    pub resolver: Arc<dyn Resolver>,
}

impl QueueConfig {
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            base: DownloadOptions::default(),
            store,
            resolver: Arc::new(DirectLinks),
        }
    }
}

impl Queue {
    /// Abre la cola con lo guardado en la base de datos y empieza a
    /// trabajar en el runtime `rt`.
    pub fn open(rt: Handle, cfg: QueueConfig) -> Result<Self, omni_store::Error> {
        let settings: QueueSettings = cfg.store.setting(SETTINGS_KEY)?.unwrap_or_default();
        let servers = Arc::new(ServerLimits::new(settings.max_per_server));
        for l in cfg.store.server_limits(SERVER_LIMITS_MAX_AGE)? {
            servers.remember(l);
        }
        let entries: Vec<Entry> = cfg
            .store
            .jobs()?
            .into_iter()
            .map(Entry::from_record)
            .collect();
        if !entries.is_empty() {
            info!(
                "{} descargas recuperadas de la base de datos",
                entries.len()
            );
        }
        let blocked = {
            let (weekday, minute) = crate::schedule::local_now();
            !settings.schedule.allows(weekday, minute)
        };
        let inner = Arc::new(Inner {
            rt,
            base: cfg.base,
            store: cfg.store,
            resolver: cfg.resolver,
            entries: Mutex::new(entries),
            global_rate: Arc::new(RateLimit::new(settings.speed_limit)),
            settings: Mutex::new(settings),
            servers,
            next_run: AtomicU64::new(0),
            closing: AtomicBool::new(false),
            sched_blocked: AtomicBool::new(blocked),
            stop_ticker: CancellationToken::new(),
        });
        let queue = Self { inner };
        queue.inner.rt.spawn(ticker(Arc::downgrade(&queue.inner)));
        queue.pump();
        Ok(queue)
    }

    fn entries(&self) -> MutexGuard<'_, Vec<Entry>> {
        self.inner.entries()
    }

    // ---- Ajustes ----

    pub fn settings(&self) -> QueueSettings {
        self.inner.settings().clone()
    }

    /// Cambia los ajustes (y los guarda). Los límites valen también para las
    /// descargas en marcha; las conexiones por archivo, para las siguientes.
    pub fn update_settings(&self, f: impl FnOnce(&mut QueueSettings)) {
        let new = {
            let mut s = self.inner.settings();
            f(&mut s);
            s.max_active = s.max_active.max(1);
            s.connections = s.connections.max(1);
            s.speed_limit = s.speed_limit.filter(|&v| v > 0);
            s.clone()
        };
        self.inner.global_rate.set(new.speed_limit);
        if self.inner.servers.max_per_server() != new.max_per_server {
            self.inner.servers.set_max_per_server(new.max_per_server);
        }
        if let Err(e) = self.inner.store.set_setting(SETTINGS_KEY, &new) {
            warn!("no se pudieron guardar los ajustes de la cola: {e}");
        }
        self.pump();
    }

    /// Descargas a la vez (las demás esperan en la cola).
    pub fn set_max_active(&self, n: usize) {
        self.update_settings(|s| s.max_active = n);
    }

    /// Conexiones por archivo para las descargas que arranquen a partir de
    /// ahora.
    pub fn set_connections(&self, n: usize) {
        self.update_settings(|s| s.connections = n);
    }

    /// Límite de velocidad global en bytes por segundo (`None` o 0 = sin
    /// límite).
    pub fn set_speed_limit(&self, bytes_per_sec: Option<u64>) {
        self.update_settings(|s| s.speed_limit = bytes_per_sec);
    }

    /// Conexiones como mucho a un mismo servidor (0 = sin tope).
    pub fn set_max_per_server(&self, n: usize) {
        self.update_settings(|s| s.max_per_server = n);
    }

    /// Servidores con los que se ha hablado: conexiones abiertas y cuántas
    /// aceptan.
    pub fn servers(&self) -> Vec<ServerStatus> {
        self.inner.servers.status()
    }

    // ---- Descargas ----

    /// Añade una descarga. Si ya está en la lista (misma URL y carpeta) y
    /// estaba parada, la vuelve a poner en marcha en vez de duplicarla.
    pub fn add(&self, new: NewJob) -> Result<u64, String> {
        let url = new.url.trim().to_owned();
        if url.is_empty() {
            return Err("Pega un enlace primero.".into());
        }
        if new.dir.as_os_str().is_empty() {
            return Err("Elige una carpeta de destino.".into());
        }
        // Validamos ya la URL para avisar sin añadir una fila que va a fallar.
        Download::new(
            &url,
            Destination::Dir(new.dir.clone()),
            self.inner.base.clone(),
        )
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
                            self.inner.save(e);
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
        let mut entry = Entry {
            id: 0,
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
            connections: self.inner.settings().connections,
            rate: Arc::new(RateLimit::new(new.speed_limit)),
            networks: Vec::new(),
            created_at: now_secs(),
            finished_at: None,
            run_id: 0,
            progress: None,
            cancel: None,
            task: None,
            speed: SpeedMeter::default(),
            detail: String::new(),
            last_total: new.size,
            last_downloaded: 0,
        };
        entry.id = self
            .inner
            .store
            .insert_job(&entry.record())
            .map_err(|e| e.to_string())?;
        let id = entry.id;
        self.entries().push(entry);
        self.pump();
        Ok(id)
    }

    /// Arranca lo que toque de la cola: primero la prioridad más alta y, a
    /// igual prioridad, lo que se añadió antes.
    pub fn pump(&self) {
        self.inner.pump();
    }

    /// Pone en la cola las descargas paradas o fallidas de `ids`.
    pub fn start(&self, ids: &[u64]) {
        for e in self.entries().iter_mut().filter(|e| ids.contains(&e.id)) {
            if matches!(e.state, State::Paused | State::Failed) {
                e.state = State::Queued;
                self.inner.save(e);
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
            self.inner.cancel_entry(e, false);
        }
        self.pump();
    }

    /// Botón "Iniciar": quita la pausa global y pone en la cola todo lo
    /// parado.
    pub fn start_all(&self) {
        self.update_settings(|s| s.held = false);
        let ids = self.ids();
        self.start(&ids);
    }

    /// Botón "Pausar": pausa lo que corre y no arranca nada más; al pulsar
    /// "Iniciar" todo sigue donde estaba.
    pub fn pause_all(&self) {
        self.update_settings(|s| s.held = true);
        for e in self.entries().iter_mut() {
            self.inner.cancel_entry(e, true);
        }
    }

    /// Botón "Detener": para todo y vacía la cola.
    pub fn stop_all(&self) {
        for e in self.entries().iter_mut() {
            self.inner.cancel_entry(e, false);
        }
        self.update_settings(|s| s.held = false);
    }

    pub fn is_held(&self) -> bool {
        self.inner.settings().held
    }

    /// ¿Está la cola parada porque ahora no toca descargar según el horario?
    pub fn blocked_by_schedule(&self) -> bool {
        self.inner.sched_blocked.load(Ordering::Relaxed)
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
        if let Err(e) = self.inner.store.delete_jobs(ids) {
            warn!("no se pudieron borrar descargas de la base de datos: {e}");
        }
        self.pump();
    }

    /// Quita descargas y además borra del disco lo suyo: el archivo (o el
    /// `.part` si iba a medias) y el estado para reanudar. Las carpetas que
    /// se queden vacías también se borran, salvo `keep` (la carpeta de
    /// descargas que eligió el usuario) y las carpetas personales del
    /// sistema. Nunca borra una carpeta con algo dentro. Devuelve cuántos
    /// archivos borró.
    pub async fn remove_with_files(&self, ids: &[u64], keep: Option<PathBuf>) -> usize {
        let mut gone = Vec::new();
        self.entries().retain_mut(|e| {
            if !ids.contains(&e.id) {
                return true;
            }
            if let Some(c) = &e.cancel {
                c.cancel();
            }
            gone.push((e.dir.clone(), e.name.clone(), e.task.take()));
            false
        });
        if let Err(e) = self.inner.store.delete_jobs(ids) {
            warn!("no se pudieron borrar descargas de la base de datos: {e}");
        }
        self.pump();
        let still_used: Vec<PathBuf> = self.entries().iter().map(|e| e.dir.clone()).collect();

        let mut deleted = 0;
        let mut dirs = Vec::new();
        for (dir, name, task) in gone {
            // Esperar a que la descarga suelte el archivo (en Windows no se
            // puede borrar abierto) y no vuelva a guardar su estado.
            if let Some(t) = task {
                let _ = t.await;
            }
            let file = dir.join(&name);
            for p in [
                file.clone(),
                omni_core::part_path_for(&file),
                omni_core::state_path_for(&file),
                omni_core::legacy_state_path_for(&file),
            ] {
                match std::fs::remove_file(&p) {
                    Ok(()) => deleted += 1,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => warn!("no se pudo borrar {}: {e}", p.display()),
                }
            }
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
        for dir in dirs {
            if still_used.contains(&dir) || protected_dir(&dir, keep.as_deref()) {
                continue;
            }
            // remove_dir solo borra carpetas vacías.
            if std::fs::remove_dir(&dir).is_ok() {
                info!("carpeta vacía borrada: {}", dir.display());
            }
        }
        deleted
    }

    pub fn set_priority(&self, ids: &[u64], priority: Priority) {
        for e in self.entries().iter_mut().filter(|e| ids.contains(&e.id)) {
            e.priority = priority;
            self.inner.save(e);
        }
    }

    /// Límite de velocidad propio de unas descargas (vale en marcha).
    pub fn set_job_speed_limit(&self, ids: &[u64], bytes_per_sec: Option<u64>) {
        for e in self.entries().iter_mut().filter(|e| ids.contains(&e.id)) {
            e.rate.set(bytes_per_sec);
            self.inner.save(e);
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

    /// ¿Hay ya una descarga con este enlace (en cualquier estado)? El
    /// capturador lo usa para avisar de duplicados.
    pub fn contains_url(&self, url: &str) -> bool {
        self.entries().iter().any(|e| e.url == url)
    }

    /// ¿Hay alguna descarga en marcha?
    pub fn any_active(&self) -> bool {
        self.entries().iter().any(|e| e.state.is_active())
    }

    /// Foto de la lista para pintarla.
    pub fn snapshot(&self) -> Vec<JobInfo> {
        self.inner.refresh();
        let held = self.is_held() || self.blocked_by_schedule();
        self.entries().iter().map(|e| info(e, held)).collect()
    }

    /// Pausa todo lo que corre y espera (como mucho `timeout`) a que el motor
    /// guarde el progreso. Lo que estaba en marcha queda en la cola para
    /// seguir solo al volver a abrir.
    pub async fn shutdown(&self, timeout: Duration) {
        self.inner.closing.store(true, Ordering::Relaxed);
        self.inner.stop_ticker.cancel();
        let tasks: Vec<_> = self
            .entries()
            .iter_mut()
            .filter_map(|e| {
                if let Some(c) = &e.cancel {
                    c.cancel();
                    e.requeue_after_pause = true;
                }
                e.task.take()
            })
            .collect();
        if !tasks.is_empty() {
            info!(
                "cerrando: guardando el progreso de {} descargas",
                tasks.len()
            );
            let _ = tokio::time::timeout(timeout, async {
                for t in tasks {
                    let _ = t.await;
                }
            })
            .await;
        }
        self.inner.save_progress();
    }
}

impl Inner {
    fn entries(&self) -> MutexGuard<'_, Vec<Entry>> {
        self.entries.lock().expect("lista de descargas envenenada")
    }

    fn settings(&self) -> MutexGuard<'_, QueueSettings> {
        self.settings.lock().expect("ajustes envenenados")
    }

    fn save(&self, e: &Entry) {
        if let Err(err) = self.store.update_job(&e.record()) {
            warn!("no se pudo guardar la descarga {}: {err}", e.id);
        }
    }

    /// Guarda el progreso de lo que corre y lo aprendido de los servidores.
    fn save_progress(&self) {
        let rows: Vec<_> = self
            .entries()
            .iter()
            .filter(|e| e.state.is_active())
            .map(|e| (e.id, e.last_downloaded, e.last_total))
            .collect();
        if let Err(e) = self.store.update_progress(&rows) {
            warn!("no se pudo guardar el progreso: {e}");
        }
        if let Err(e) = self.store.save_server_limits(&self.servers.learned()) {
            warn!("no se pudo guardar lo aprendido de los servidores: {e}");
        }
    }

    /// Mira el horario: al salir de él pausa lo que corre y al volver a entrar
    /// la cola sigue sola.
    fn check_schedule(&self) {
        let schedule = self.settings().schedule;
        let (weekday, minute) = crate::schedule::local_now();
        let blocked = !schedule.allows(weekday, minute);
        if self.sched_blocked.swap(blocked, Ordering::Relaxed) == blocked {
            return;
        }
        if blocked {
            info!("fuera del horario de descargas: se pausa la cola");
            for e in self.entries().iter_mut() {
                self.cancel_entry(e, true);
            }
        } else {
            info!("empieza el horario de descargas: la cola sigue");
        }
    }

    fn pump(self: &Arc<Self>) {
        if self.closing.load(Ordering::Relaxed) {
            return;
        }
        let (held, max) = {
            let s = self.settings();
            (s.held, s.max_active)
        };
        if held || self.sched_blocked.load(Ordering::Relaxed) {
            return;
        }
        let mut entries = self.entries();
        loop {
            let active = entries.iter().filter(|e| e.state.is_active()).count();
            if active >= max {
                return;
            }
            let next = entries
                .iter_mut()
                .filter(|e| e.state == State::Queued)
                .min_by_key(|e| (e.priority, e.id));
            match next {
                Some(e) => self.launch(e),
                None => return,
            }
        }
    }

    /// Lanza (o relanza) una descarga.
    fn launch(self: &Arc<Self>, e: &mut Entry) {
        let run_id = self.next_run.fetch_add(1, Ordering::Relaxed) + 1;
        let connections = self.settings().connections;
        e.connections = connections;
        let mut opts = DownloadOptions {
            connections_per_network: connections,
            ..self.base.clone()
        };
        opts.rate_limits
            .extend([self.global_rate.clone(), e.rate.clone()]);
        opts.server_limits = Some(self.servers.clone());

        let cancel = CancellationToken::new();
        // El progreso existe antes de saber el enlace directo, para que la
        // interfaz tenga algo que leer desde el principio.
        let progress = Arc::new(Progress::default());
        e.state = State::Starting;
        e.started_once = true;
        e.requeue_after_pause = false;
        e.run_id = run_id;
        e.detail.clear();
        e.speed.reset();
        e.progress = Some(progress.clone());
        e.cancel = Some(cancel.clone());
        e.finished_at = None;
        self.save(e);
        info!(url = %e.url, "descarga lanzada");

        let me = Arc::clone(self);
        let url = e.url.clone();
        let dir = e.dir.clone();
        e.task = Some(self.rt.spawn(async move {
            let result = me.run_one(&url, dir, opts, cancel, run_id, progress).await;
            me.finished(run_id, result);
        }));
    }

    async fn run_one(
        &self,
        url: &str,
        dir: PathBuf,
        mut opts: DownloadOptions,
        cancel: CancellationToken,
        run_id: u64,
        shown: Arc<Progress>,
    ) -> omni_core::Result<omni_core::DownloadOutcome> {
        let link = tokio::select! {
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            r = self.resolver.resolve(url, &cancel) => r?,
        };
        if let Some(engine) = link.custom {
            return engine.run(&dir, shown, cancel).await;
        }
        if let Some(job) = link.external {
            // Lo descarga un programa externo; el progreso que ve la
            // interfaz es el que él va diciendo.
            return crate::external::run(job, &dir, shown, cancel).await;
        }
        opts.headers.extend(link.headers);
        let dest = match link.filename {
            Some(name) => Destination::File(dir.join(omni_core::filename::sanitize(&name))),
            None => Destination::Dir(dir),
        };
        let download = Download::new(&link.url, dest, opts)?;
        // Desde aquí la interfaz lee el progreso del motor.
        let progress = download.progress();
        drop(shown);
        if let Some(e) = self.entries().iter_mut().find(|e| e.run_id == run_id) {
            e.progress = Some(progress);
        }
        download.run(cancel).await
    }

    fn finished(
        self: &Arc<Self>,
        run_id: u64,
        result: omni_core::Result<omni_core::DownloadOutcome>,
    ) {
        {
            let mut entries = self.entries();
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
                    e.finished_at = Some(now_secs());
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
            self.save(e);
        }
        if let Err(e) = self.store.save_server_limits(&self.servers.learned()) {
            warn!("no se pudo guardar lo aprendido de los servidores: {e}");
        }
        self.pump();
    }

    fn cancel_entry(&self, e: &mut Entry, requeue: bool) {
        match e.state {
            State::Starting | State::Running => {
                if let Some(c) = &e.cancel {
                    c.cancel();
                }
                e.state = State::Pausing;
                e.requeue_after_pause = requeue;
            }
            State::Pausing => e.requeue_after_pause = requeue,
            State::Queued if !requeue => {
                e.state = State::Paused;
                self.save(e);
            }
            _ => {}
        }
    }

    /// Pone al día lo que se ve de cada descarga activa: bytes, tamaño,
    /// velocidad y si ya pasó de "conectando" a "descargando".
    fn refresh(&self) {
        let now = Instant::now();
        for e in self.entries().iter_mut() {
            let Some(p) = &e.progress else { continue };
            if !e.state.is_active() {
                continue;
            }
            e.last_total = p.total().or(e.last_total);
            e.last_downloaded = p.downloaded();
            // En cuanto llegan datos o se conoce el tamaño, deja de "empezar".
            if e.state == State::Starting && (p.total().is_some() || p.downloaded() > 0) {
                e.state = State::Running;
            }
            if matches!(e.state, State::Starting | State::Running) {
                e.speed.sample(now, e.last_downloaded, p.resumed_from());
            }
        }
    }
}

/// Revisa la cola cada poco: arranca lo que toque, mide velocidades y, de vez
/// en cuando, guarda el progreso. Termina sola cuando la cola desaparece.
async fn ticker(inner: std::sync::Weak<Inner>) {
    let mut iv = tokio::time::interval(TICK);
    iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut n = 0u32;
    loop {
        let stop = match inner.upgrade() {
            Some(i) => i.stop_ticker.clone(),
            None => return,
        };
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = iv.tick() => {}
        }
        let Some(inner) = inner.upgrade() else { return };
        inner.check_schedule();
        inner.refresh();
        inner.pump();
        n += 1;
        if n.is_multiple_of(SAVE_EVERY) {
            inner.save_progress();
        }
    }
}

fn info(e: &Entry, held: bool) -> JobInfo {
    let mut receiving = 0;
    let mut server_limit = 0;
    let mut retries = 0;
    if let Some(p) = &e.progress
        && e.state.is_active()
    {
        let stats = p.per_network();
        receiving = stats.values().map(|s| s.receiving).sum();
        server_limit = stats.values().map(|s| s.limit).sum();
        retries = stats.values().map(|s| s.retries).sum();
    }
    let rate = match e.state {
        State::Starting | State::Running => e.speed.rate,
        _ => 0.0,
    };
    JobInfo {
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
        downloaded: e.last_downloaded,
        total: e.last_total,
        rate,
        receiving,
        connections: e.connections,
        server_limit,
        retries,
        started_once: e.started_once,
        speed_limit: e.rate.get(),
        detail: e.detail.clone(),
    }
}

/// Carpetas que nunca se borran aunque se queden vacías.
fn protected_dir(dir: &std::path::Path, keep: Option<&std::path::Path>) -> bool {
    let same = |other: &std::path::Path| {
        let a = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let b = std::fs::canonicalize(other).unwrap_or_else(|_| other.to_path_buf());
        a == b
    };
    keep.is_some_and(same)
        || [
            dirs::download_dir(),
            dirs::home_dir(),
            dirs::desktop_dir(),
            dirs::document_dir(),
            dirs::video_dir(),
            dirs::audio_dir(),
            dirs::picture_dir(),
        ]
        .into_iter()
        .flatten()
        .any(|d| same(&d))
        || dir.parent().is_none()
}

#[cfg(test)]
mod tests {
    use super::SpeedMeter;
    use std::time::{Duration, Instant};

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
