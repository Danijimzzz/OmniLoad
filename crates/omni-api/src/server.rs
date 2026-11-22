//! El servicio: la cola y su API HTTP.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use omni_core::{DownloadOptions, Network, ServerStatus};
use omni_grab::{Added, Checkers, GrabCommand, Grabber};
use omni_queue::{JobInfo, NewJob, Queue, QueueConfig, QueueSettings};
use omni_store::Store;
use tokio::net::TcpListener;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::paths::{Endpoint, Paths};
use crate::types::{
    AddLinksRequest, AddResponse, ErrorResponse, GrabberView, Ids, PriorityRequest, RemoveRequest,
    SpeedLimitRequest, Status, TakeRequest, TakeResponse,
};
use crate::types::{DeltaRequest, JobsDelta};

/// Errores al arrancar el servicio.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("ya hay un servicio de OmniLoad en marcha con esta base de datos")]
    AlreadyRunning,
    #[error("{0}")]
    Store(#[from] omni_store::Error),
    #[error("error de E/S: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Network(#[from] omni_core::Error),
}

/// Tiempo máximo para que las descargas guarden su progreso al parar.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// Cada cuánto se guarda el capturador, si cambió.
const GRABBER_SAVE_EVERY: Duration = Duration::from_secs(2);
/// Clave del capturador en la base de datos.
const GRABBER_KEY: &str = "capturador";

#[derive(Clone)]
struct App {
    queue: Queue,
    grabber: Grabber,
    token: Arc<str>,
    stop: CancellationToken,
    views: Arc<Mutex<HashMap<String, View>>>,
}

/// Lo último que se le mandó a una ventana.
struct View {
    seen: Instant,
    order: Vec<u64>,
    jobs: HashMap<u64, JobInfo>,
}

/// Una ventana que lleva este tiempo sin preguntar se olvida.
const VIEW_TTL: Duration = Duration::from_secs(60);

/// El servicio en marcha.
pub struct Service {
    queue: Queue,
    grabber: Grabber,
    store: Arc<Store>,
    saver: JoinHandle<()>,
    stop: CancellationToken,
    server: JoinHandle<()>,
    paths: Paths,
    port: u16,
    /// Mientras viva, nadie más puede arrancar otro servicio con la misma
    /// base de datos.
    _lock: File,
}

impl Service {
    /// Arranca el servicio: candado, base de datos, cola y API. Hay que
    /// llamarlo dentro de un runtime de tokio.
    pub async fn start(paths: &Paths, base: DownloadOptions) -> Result<Self, ServiceError> {
        std::fs::create_dir_all(paths.dir())?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&paths.lock)?;
        if fs4::FileExt::try_lock(&lock).is_err() {
            return Err(ServiceError::AlreadyRunning);
        }

        let store = Arc::new(Store::open(&paths.db)?);
        let mut cfg = QueueConfig::new(store.clone());
        cfg.base = base;
        let queue = Queue::open(Handle::current(), cfg)?;
        let grabber = open_grabber(&queue, &store)?;

        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        let token = new_token()?;
        let stop = CancellationToken::new();
        let app = App {
            queue: queue.clone(),
            grabber: grabber.clone(),
            token: token.clone().into(),
            stop: stop.clone(),
            views: Arc::default(),
        };
        let server = {
            let stop = stop.clone();
            tokio::spawn(async move {
                let res = axum::serve(listener, router(app))
                    .with_graceful_shutdown(async move { stop.cancelled().await })
                    .await;
                if let Err(e) = res {
                    warn!("la API se cerró con error: {e}");
                }
            })
        };
        Endpoint {
            port,
            token,
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        }
        .write(&paths.endpoint)?;
        info!("servicio en marcha en 127.0.0.1:{port}");
        let saver = tokio::spawn(save_grabber_loop(
            grabber.clone(),
            store.clone(),
            stop.clone(),
        ));
        Ok(Self {
            queue,
            grabber,
            store,
            saver,
            stop,
            server,
            paths: paths.clone(),
            port,
            _lock: lock,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn queue(&self) -> &Queue {
        &self.queue
    }

    pub fn grabber(&self) -> &Grabber {
        &self.grabber
    }

    /// Se cancela cuando alguien pide parar el servicio por la API.
    pub fn stop_requested(&self) -> CancellationToken {
        self.stop.clone()
    }

    /// Para la API, guarda el progreso de las descargas y suelta el candado.
    pub async fn stop(self) {
        self.stop.cancel();
        let _ = self.server.await;
        let _ = self.saver.await;
        save_grabber(&self.grabber, &self.store);
        self.queue.shutdown(SHUTDOWN_GRACE).await;
        let _ = std::fs::remove_file(&self.paths.endpoint);
        info!("servicio parado");
    }
}

/// El capturador, con lo que quedó la última vez. Avisa de los enlaces que
/// ya están en la lista de descargas.
fn open_grabber(queue: &Queue, store: &Store) -> Result<Grabber, ServiceError> {
    let net = Network::system_default()?;
    let known = queue.clone();
    // Los plugins de hosts (fase 5) se añadirán aquí con `Checkers::with`.
    let grabber = Grabber::new(Handle::current(), net.client().clone(), Checkers::new())
        .with_known_links(Arc::new(move |url| known.contains_url(url)));
    match store.setting(GRABBER_KEY) {
        Ok(Some(saved)) => grabber.restore(saved),
        Ok(None) => {}
        Err(e) => warn!("no se pudo leer el capturador guardado: {e}"),
    }
    Ok(grabber)
}

fn save_grabber(grabber: &Grabber, store: &Store) {
    if let Err(e) = store.set_setting(GRABBER_KEY, &grabber.save()) {
        warn!("no se pudo guardar el capturador: {e}");
    }
}

/// Guarda el capturador cuando cambia (como mucho cada dos segundos).
async fn save_grabber_loop(grabber: Grabber, store: Arc<Store>, stop: CancellationToken) {
    let mut saved = grabber.generation();
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(GRABBER_SAVE_EVERY) => {}
        }
        let now = grabber.generation();
        if now != saved {
            saved = now;
            let (g, s) = (grabber.clone(), store.clone());
            let _ = tokio::task::spawn_blocking(move || save_grabber(&g, &s)).await;
        }
    }
}

/// Token aleatorio de 256 bits en hexadecimal.
fn new_token() -> std::io::Result<String> {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(hex::encode(buf))
}

fn router(app: App) -> Router {
    Router::new()
        .route("/api/v1/status", get(status))
        .route("/api/v1/jobs", get(jobs).post(add))
        .route("/api/v1/jobs/changes", post(jobs_changes))
        .route("/api/v1/jobs/start", post(start))
        .route("/api/v1/jobs/stop", post(stop))
        .route("/api/v1/jobs/remove", post(remove))
        .route("/api/v1/jobs/priority", post(priority))
        .route("/api/v1/jobs/speed-limit", post(job_speed_limit))
        .route("/api/v1/queue/start-all", post(start_all))
        .route("/api/v1/queue/pause-all", post(pause_all))
        .route("/api/v1/queue/stop-all", post(stop_all))
        .route("/api/v1/settings", get(settings).put(set_settings))
        .route("/api/v1/servers", get(servers))
        .route("/api/v1/grabber", get(grabber))
        .route("/api/v1/grabber/links", post(grabber_links))
        .route("/api/v1/grabber/command", post(grabber_command))
        .route("/api/v1/grabber/take", post(grabber_take))
        .route("/api/v1/shutdown", post(shutdown))
        .layer(middleware::from_fn_with_state(app.clone(), auth))
        .with_state(app)
}

/// Cada petición debe llevar `Authorization: Bearer <token>`.
async fn auth(State(app): State<App>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| constant_time_eq(t.as_bytes(), app.token.as_bytes()));
    if !ok {
        return error(StatusCode::UNAUTHORIZED, "falta el token o no es válido");
    }
    next.run(req).await
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn error(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(ErrorResponse { error: msg.into() })).into_response()
}

async fn status(State(app): State<App>) -> Json<Status> {
    Json(Status {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        pid: std::process::id(),
        settings: app.queue.settings(),
    })
}

async fn jobs(State(app): State<App>) -> Json<Vec<JobInfo>> {
    Json(app.queue.snapshot())
}

async fn jobs_changes(State(app): State<App>, Json(req): Json<DeltaRequest>) -> Json<JobsDelta> {
    let snapshot = app.queue.snapshot();
    let now = Instant::now();
    let mut views = app.views.lock().expect("vistas envenenadas");
    views.retain(|_, v| now.duration_since(v.seen) < VIEW_TTL);
    let order: Vec<u64> = snapshot.iter().map(|j| j.id).collect();
    let prev = if req.reset {
        None
    } else {
        views.remove(&req.client)
    };
    let delta = match &prev {
        None => JobsDelta {
            order: Some(order.clone()),
            jobs: snapshot.clone(),
        },
        Some(v) => JobsDelta {
            order: (v.order != order).then(|| order.clone()),
            jobs: snapshot
                .iter()
                .filter(|j| v.jobs.get(&j.id) != Some(*j))
                .cloned()
                .collect(),
        },
    };
    views.insert(
        req.client,
        View {
            seen: now,
            order,
            jobs: snapshot.into_iter().map(|j| (j.id, j)).collect(),
        },
    );
    Json(delta)
}

async fn add(State(app): State<App>, Json(new): Json<NewJob>) -> Response {
    match app.queue.add(new) {
        Ok(id) => Json(AddResponse { id }).into_response(),
        Err(e) => error(StatusCode::BAD_REQUEST, e),
    }
}

async fn start(State(app): State<App>, Json(r): Json<Ids>) -> StatusCode {
    app.queue.start(&r.ids);
    StatusCode::NO_CONTENT
}

async fn stop(State(app): State<App>, Json(r): Json<Ids>) -> StatusCode {
    app.queue.stop(&r.ids);
    StatusCode::NO_CONTENT
}

async fn remove(State(app): State<App>, Json(r): Json<RemoveRequest>) -> StatusCode {
    if r.files {
        app.queue.remove_with_files(&r.ids, r.keep).await;
    } else {
        app.queue.remove(&r.ids);
    }
    StatusCode::NO_CONTENT
}

async fn priority(State(app): State<App>, Json(r): Json<PriorityRequest>) -> StatusCode {
    app.queue.set_priority(&r.ids, r.priority);
    StatusCode::NO_CONTENT
}

async fn job_speed_limit(State(app): State<App>, Json(r): Json<SpeedLimitRequest>) -> StatusCode {
    app.queue.set_job_speed_limit(&r.ids, r.bytes_per_sec);
    StatusCode::NO_CONTENT
}

async fn start_all(State(app): State<App>) -> StatusCode {
    app.queue.start_all();
    StatusCode::NO_CONTENT
}

async fn pause_all(State(app): State<App>) -> StatusCode {
    app.queue.pause_all();
    StatusCode::NO_CONTENT
}

async fn stop_all(State(app): State<App>) -> StatusCode {
    app.queue.stop_all();
    StatusCode::NO_CONTENT
}

async fn settings(State(app): State<App>) -> Json<QueueSettings> {
    Json(app.queue.settings())
}

async fn set_settings(
    State(app): State<App>,
    Json(new): Json<QueueSettings>,
) -> Json<QueueSettings> {
    app.queue.update_settings(|s| *s = new);
    Json(app.queue.settings())
}

async fn servers(State(app): State<App>) -> Json<Vec<ServerStatus>> {
    Json(app.queue.servers())
}

async fn shutdown(State(app): State<App>) -> StatusCode {
    info!("se pidió parar el servicio");
    app.stop.cancel();
    StatusCode::NO_CONTENT
}

async fn grabber(State(app): State<App>) -> Json<GrabberView> {
    Json(GrabberView {
        packages: app.grabber.snapshot(),
        notes: app.grabber.take_notes(),
        hosts: app.grabber.hosts(),
    })
}

async fn grabber_links(State(app): State<App>, Json(r): Json<AddLinksRequest>) -> Response {
    let res = if r.new_batch {
        app.grabber.add_links(&r.text, true)
    } else {
        app.grabber
            .add_to_batch(&r.text, r.batch)
            .map(|added| Added {
                added,
                batch: r.batch,
            })
    };
    match res {
        Ok(added) => Json(added).into_response(),
        Err(e) => error(StatusCode::BAD_REQUEST, e),
    }
}

async fn grabber_command(State(app): State<App>, Json(cmd): Json<GrabCommand>) -> StatusCode {
    app.grabber.apply(cmd);
    StatusCode::NO_CONTENT
}

/// Pasa archivos del capturador a la lista de descargas.
async fn grabber_take(State(app): State<App>, Json(r): Json<TakeRequest>) -> Json<TakeResponse> {
    let mut out = TakeResponse::default();
    for picked in app.grabber.take(r.pick, &r.dest) {
        let name = picked.name.clone();
        match app.queue.add(picked.into_job(r.start)) {
            Ok(id) => out.ids.push(id),
            Err(e) => out.errors.push(format!("{name}: {e}.")),
        }
    }
    if r.start && !out.ids.is_empty() {
        app.queue.start_all();
    }
    Json(out)
}
