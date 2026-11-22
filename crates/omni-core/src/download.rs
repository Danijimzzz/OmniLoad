//! El motor: sondea, reserva el archivo, lanza las conexiones sobre la cola de
//! bloques, guarda el progreso y cierra la descarga.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_RANGE, HeaderMap, IF_RANGE, RANGE};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use url::Url;

use crate::blocks::{BlockQueue, Commit, Lease};
use crate::limiter::{Limiter, REGROW_AFTER};
use crate::probe::{parse_content_range, probe_with, status_error};
use crate::retry::{Backoff, retry_after};
use crate::state::{SavedState, legacy_state_path_for, part_path_for, state_path_for};
use crate::{
    DEFAULT_BLOCK_SIZE, DEFAULT_CONNECTIONS_PER_NETWORK, DEFAULT_MIN_BLOCK_SIZE, Error, Network,
    NetworkId, Progress, RateLimit, RemoteInfo, Result, ServerLimits, platform,
};

/// Cada cuánto vuelca una conexión su búfer a disco aunque no esté lleno.
/// Junto con `save_interval` acota el progreso que se pierde en un corte.
const FLUSH_INTERVAL: Duration = Duration::from_millis(250);

/// Número de cada descarga en marcha, para repartir los límites compartidos.
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

/// Dónde guardar la descarga.
#[derive(Clone, Debug)]
pub enum Destination {
    /// Ruta exacta del archivo final.
    File(PathBuf),
    /// Carpeta; el nombre sale del servidor o de la URL.
    Dir(PathBuf),
}

/// Ajustes de una descarga.
#[derive(Clone, Debug)]
pub struct DownloadOptions {
    /// Redes por las que descargar. Vacío = la red por defecto del sistema.
    pub networks: Vec<Network>,
    /// Conexiones simultáneas por red.
    pub connections_per_network: usize,
    /// Tamaño de los bloques de la cola.
    pub block_size: u64,
    /// Tamaño mínimo de bloque al partir en el final rápido.
    pub min_block_size: u64,
    /// Búfer de escritura por conexión.
    pub write_buffer: usize,
    /// Fallos seguidos sin progreso antes de rendirse.
    pub max_retries: u32,
    /// Primera espera entre reintentos (luego se dobla).
    pub retry_base: Duration,
    /// Espera máxima entre reintentos.
    pub retry_max: Duration,
    /// Cada cuánto se guarda el progreso en disco.
    pub save_interval: Duration,
    /// SHA-256 esperado (hexadecimal); se comprueba al terminar.
    pub expected_sha256: Option<String>,
    /// Sobrescribir el destino si ya existe.
    pub overwrite: bool,
    /// Límites de velocidad que respeta esta descarga (por ejemplo, uno
    /// global compartido y otro propio). Vacío = sin límite.
    pub rate_limits: Vec<Arc<RateLimit>>,
    /// Límites de conexiones por servidor compartidos con otras descargas.
    /// `None` = esta descarga lleva su propio límite adaptativo.
    pub server_limits: Option<Arc<ServerLimits>>,
    /// Cabeceras extra en todas las peticiones (cookies o tokens que pida un
    /// host; lo usarán los plugins de hosts).
    pub headers: HeaderMap,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            networks: Vec::new(),
            connections_per_network: DEFAULT_CONNECTIONS_PER_NETWORK,
            block_size: DEFAULT_BLOCK_SIZE,
            min_block_size: DEFAULT_MIN_BLOCK_SIZE,
            write_buffer: 1024 * 1024,
            max_retries: 10,
            retry_base: Duration::from_secs(1),
            retry_max: Duration::from_secs(60),
            save_interval: Duration::from_millis(500),
            expected_sha256: None,
            overwrite: false,
            rate_limits: Vec::new(),
            server_limits: None,
            headers: HeaderMap::new(),
        }
    }
}

impl DownloadOptions {
    fn backoff(&self) -> Backoff {
        Backoff {
            base: self.retry_base,
            max: self.retry_max,
        }
    }

    /// Espera lo que pidan los límites de velocidad por `bytes` recibidos.
    /// `false` si se canceló.
    async fn throttle(&self, bytes: u64, cancel: &CancellationToken) -> bool {
        for limit in &self.rate_limits {
            if !limit.consume(bytes, cancel).await {
                return false;
            }
        }
        true
    }
}

/// Resultado de una descarga terminada.
#[derive(Clone, Debug)]
pub struct DownloadOutcome {
    pub path: PathBuf,
    pub size: u64,
    /// Bytes que ya estaban descargados al empezar (reanudación).
    pub resumed_from: u64,
    /// ¿Se descargó por bloques? `false` si el servidor no acepta rangos.
    pub segmented: bool,
    /// SHA-256 calculado, si se pidió comprobarlo.
    pub sha256: Option<String>,
}

/// Una descarga lista para lanzar.
pub struct Download {
    url: Url,
    dest: Destination,
    opts: DownloadOptions,
    progress: Arc<Progress>,
}

impl Download {
    pub fn new(url: &str, dest: Destination, mut opts: DownloadOptions) -> Result<Self> {
        let url = Url::parse(url).map_err(|e| Error::InvalidUrl(format!("{url}: {e}")))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(Error::InvalidUrl(format!(
                "solo se admiten http y https, no {}",
                url.scheme()
            )));
        }
        if opts.networks.is_empty() {
            opts.networks.push(Network::system_default()?);
        }
        opts.connections_per_network = opts.connections_per_network.max(1);
        opts.block_size = opts.block_size.max(1);
        opts.min_block_size = opts.min_block_size.clamp(1, opts.block_size);
        opts.write_buffer = opts.write_buffer.max(16 * 1024);
        Ok(Self {
            url,
            dest,
            opts,
            progress: Arc::new(Progress::default()),
        })
    }

    /// Progreso compartido; se puede leer mientras corre la descarga.
    pub fn progress(&self) -> Arc<Progress> {
        self.progress.clone()
    }

    /// Ejecuta la descarga hasta terminar, fallar o cancelarse. Al cancelar se
    /// guarda el progreso y basta con volver a lanzarla para reanudar.
    pub async fn run(self, cancel: CancellationToken) -> Result<DownloadOutcome> {
        let info = self.probe_with_retries(&cancel).await?;
        debug!(?info, "sondeo terminado");
        let path = match &self.dest {
            Destination::File(p) => p.clone(),
            Destination::Dir(d) => d.join(&info.filename),
        };
        let part = part_path_for(&path);
        let state = state_path_for(&path);
        adopt_legacy_state(&path, &state);
        if path.exists() && !part.exists() && !self.opts.overwrite {
            return Err(Error::AlreadyExists(path.display().to_string()));
        }
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        self.progress.set_total(info.size);

        let segmented = info.is_segmentable();
        if segmented {
            self.run_segmented(&info, &part, &state, &cancel).await?;
        } else {
            info!("el servidor no acepta rangos: descarga en un solo flujo, sin reanudación");
            self.run_single(&info, &part, &cancel).await?;
            let _ = fs::remove_file(&state);
        }

        self.finish(&info, &path, &part, &state, segmented).await
    }

    async fn probe_with_retries(&self, cancel: &CancellationToken) -> Result<RemoteInfo> {
        let client = self.opts.networks[0].client();
        let mut attempt = 0;
        loop {
            let res = tokio::select! {
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                r = probe_with(client, &self.url, &self.opts.headers) => r,
            };
            match res {
                Ok(info) => return Ok(info),
                Err(e) if e.is_retriable() && attempt < self.opts.max_retries => {
                    let delay = self.opts.backoff().delay(attempt);
                    warn!("sondeo fallido ({e}); reintento en {delay:?}");
                    attempt += 1;
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(Error::Cancelled),
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
                Err(e) if e.is_retriable() => return Err(Error::RetriesExhausted(Box::new(e))),
                Err(e) => return Err(e),
            }
        }
    }

    async fn run_segmented(
        &self,
        info: &RemoteInfo,
        part: &Path,
        state_path: &Path,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let size = info.size.expect("segmentable implica tamaño conocido");
        let (queue, file) = match self.try_resume(info, size, part, state_path) {
            Some(resumed) => resumed,
            None => {
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(part)?;
                platform::preallocate(&file, size)?;
                let queue = BlockQueue::new(size, self.opts.block_size, self.opts.min_block_size);
                (queue, file)
            }
        };
        self.progress.reset(queue.written());

        let owner = NEXT_OWNER.fetch_add(1, Ordering::Relaxed);
        let shared = Arc::new(Shared {
            owner,
            queue: Mutex::new(queue),
            file: Arc::new(file),
            info: info.clone(),
            opts: self.opts.clone(),
            progress: self.progress.clone(),
            abort: cancel.child_token(),
            fatal: Mutex::new(None),
            state_path: state_path.to_path_buf(),
            limits: self
                .opts
                .networks
                .iter()
                .map(|n| {
                    let limiter = match (&self.opts.server_limits, info.url.host_str()) {
                        (Some(servers), Some(host)) => servers.limiter(host, n.name()),
                        _ => Arc::new(Limiter::new(
                            self.opts.connections_per_network,
                            REGROW_AFTER,
                        )),
                    };
                    limiter.register(owner);
                    self.progress.set_limit(n.id(), limiter.allowed());
                    (n.id(), limiter)
                })
                .collect(),
        });

        // Estado inicial en disco antes de bajar nada.
        shared.save().await?;

        let stop_saver = CancellationToken::new();
        let saver = tokio::spawn(saver_loop(shared.clone(), stop_saver.clone()));

        let mut workers = JoinSet::new();
        for net in &self.opts.networks {
            for _ in 0..self.opts.connections_per_network {
                workers.spawn(worker(shared.clone(), net.clone()));
            }
        }
        while let Some(res) = workers.join_next().await {
            if let Err(e) = res {
                shared.fail(Error::Io(io::Error::other(format!(
                    "una conexión terminó de forma inesperada: {e}"
                ))));
            }
        }
        stop_saver.cancel();
        let _ = saver.await;
        for limiter in shared.limits.values() {
            limiter.unregister(owner);
        }

        let complete = shared.queue().is_complete();
        if !complete {
            // Último guardado para no perder nada al reanudar.
            if let Err(e) = shared.save().await {
                warn!("no se pudo guardar el progreso: {e}");
            }
        }
        if let Some(e) = shared.fatal.lock().expect("estado envenenado").take() {
            return Err(e);
        }
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if !complete {
            return Err(Error::Io(io::Error::other(
                "las conexiones terminaron sin completar el archivo",
            )));
        }
        let file = shared.file.clone();
        drop(shared);
        tokio::task::spawn_blocking(move || file.sync_all())
            .await
            .map_err(io::Error::other)??;
        Ok(())
    }

    /// Intenta reanudar desde el estado guardado. Devuelve `None` si no hay
    /// estado, está dañado o el archivo remoto cambió.
    fn try_resume(
        &self,
        info: &RemoteInfo,
        size: u64,
        part: &Path,
        state_path: &Path,
    ) -> Option<(BlockQueue, File)> {
        let saved = SavedState::load(state_path)?;
        if !saved.matches(size, info.etag.as_deref(), info.last_modified.as_deref()) {
            info!("el archivo remoto cambió desde la última vez: se empieza de cero");
            return None;
        }
        if !BlockQueue::validate(&saved.blocks, size) {
            warn!("estado de reanudación inconsistente: se empieza de cero");
            return None;
        }
        let file = OpenOptions::new().read(true).write(true).open(part).ok()?;
        if file.metadata().ok()?.len() != size {
            warn!("el archivo .part no tiene el tamaño esperado: se empieza de cero");
            return None;
        }
        info!(
            "reanudando: {} de {} bytes ya descargados",
            saved.written(),
            size
        );
        Some((
            BlockQueue::from_blocks(saved.blocks, self.opts.min_block_size),
            file,
        ))
    }

    /// Servidor sin rangos o tamaño desconocido: un solo flujo de principio a
    /// fin. Si se corta, se vuelve a empezar.
    async fn run_single(
        &self,
        info: &RemoteInfo,
        part: &Path,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let net = &self.opts.networks[0];
        let mut attempt = 0;
        loop {
            self.progress.reset(0);
            match self.stream_whole(net, info, part, cancel).await {
                Ok(()) => return Ok(()),
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(e) if e.is_retriable() && attempt < self.opts.max_retries => {
                    let delay = self.opts.backoff().delay(attempt);
                    warn!("descarga cortada ({e}); se reinicia en {delay:?}");
                    attempt += 1;
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(Error::Cancelled),
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
                Err(e) if e.is_retriable() => return Err(Error::RetriesExhausted(Box::new(e))),
                Err(e) => return Err(e),
            }
        }
    }

    async fn stream_whole(
        &self,
        net: &Network,
        info: &RemoteInfo,
        part: &Path,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let resp = tokio::select! {
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            r = net.client().get(info.url.clone()).headers(self.opts.headers.clone()).send() => r?,
        };
        if resp.status() != StatusCode::OK {
            return Err(status_error(resp.status()));
        }
        let mut file = tokio::fs::File::create(part).await?;
        let mut stream = resp.bytes_stream();
        let mut received = 0u64;
        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                c = stream.next() => c,
            };
            match chunk {
                Some(Ok(bytes)) => {
                    file.write_all(&bytes).await?;
                    received += bytes.len() as u64;
                    self.progress.add(net.id(), bytes.len() as u64);
                    if !self.opts.throttle(bytes.len() as u64, cancel).await {
                        return Err(Error::Cancelled);
                    }
                }
                Some(Err(e)) => return Err(e.into()),
                None => break,
            }
        }
        file.flush().await?;
        file.sync_all().await?;
        if let Some(expected) = info.size
            && received != expected
        {
            return Err(Error::ShortBody { received, expected });
        }
        Ok(())
    }

    /// Comprueba tamaño y hash, renombra `.part` al nombre final y borra el
    /// estado.
    async fn finish(
        &self,
        info: &RemoteInfo,
        path: &Path,
        part: &Path,
        state: &Path,
        segmented: bool,
    ) -> Result<DownloadOutcome> {
        let actual = fs::metadata(part)?.len();
        if let Some(expected) = info.size
            && actual != expected
        {
            return Err(Error::SizeMismatch { expected, actual });
        }
        let sha256 = match &self.opts.expected_sha256 {
            Some(expected) => {
                let part_owned = part.to_path_buf();
                let actual_hash = tokio::task::spawn_blocking(move || sha256_file(&part_owned))
                    .await
                    .map_err(io::Error::other)??;
                if !actual_hash.eq_ignore_ascii_case(expected.trim()) {
                    // El archivo no es el esperado: no tiene sentido reanudarlo.
                    let _ = fs::remove_file(state);
                    return Err(Error::HashMismatch {
                        expected: expected.trim().to_ascii_lowercase(),
                        actual: actual_hash,
                    });
                }
                Some(actual_hash)
            }
            None => None,
        };
        fs::rename(part, path)?;
        let _ = fs::remove_file(state);
        Ok(DownloadOutcome {
            path: path.to_path_buf(),
            size: actual,
            resumed_from: self.progress.resumed_from(),
            segmented,
            sha256,
        })
    }
}

/// SHA-256 de un archivo, en hexadecimal.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut f = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Estado compartido por las conexiones de una descarga por bloques.
struct Shared {
    /// Número de esta descarga en los límites compartidos.
    owner: u64,
    queue: Mutex<BlockQueue>,
    file: Arc<File>,
    info: RemoteInfo,
    opts: DownloadOptions,
    progress: Arc<Progress>,
    /// Se cancela si el usuario cancela o si una conexión falla sin remedio.
    abort: CancellationToken,
    fatal: Mutex<Option<Error>>,
    state_path: PathBuf,
    /// Límite adaptativo de conexiones de cada red (compartido con otras
    /// descargas al mismo servidor si hay [`ServerLimits`]).
    limits: HashMap<NetworkId, Arc<Limiter>>,
}

impl Shared {
    fn queue(&self) -> MutexGuard<'_, BlockQueue> {
        self.queue.lock().expect("cola envenenada")
    }

    /// Error sin remedio: se guarda el primero y se paran todas las
    /// conexiones.
    fn fail(&self, e: Error) {
        let mut fatal = self.fatal.lock().expect("estado envenenado");
        if fatal.is_none() {
            warn!("la descarga falla: {e}");
            *fatal = Some(e);
        }
        self.abort.cancel();
    }

    /// Guarda el progreso. Primero se toma la foto de la cola y después se
    /// hace `fsync` del archivo, así el estado nunca dice que hay en disco
    /// algo que todavía no ha llegado.
    async fn save(&self) -> io::Result<()> {
        let blocks = self.queue().snapshot();
        let state = SavedState::new(
            self.info.url.to_string(),
            self.info.size.unwrap_or(0),
            self.info.etag.clone(),
            self.info.last_modified.clone(),
            blocks,
        );
        let file = self.file.clone();
        let path = self.state_path.clone();
        tokio::task::spawn_blocking(move || {
            file.sync_data()?;
            state.save_atomic(&path)
        })
        .await
        .map_err(io::Error::other)?
    }
}

async fn saver_loop(shared: Arc<Shared>, stop: CancellationToken) {
    let mut ticker = tokio::time::interval(shared.opts.save_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;
    loop {
        tokio::select! {
            _ = stop.cancelled() => break,
            _ = ticker.tick() => {}
        }
        if let Err(e) = shared.save().await {
            warn!("no se pudo guardar el progreso: {e}");
        }
    }
}

/// Una conexión: coge bloques de la cola hasta que no quede trabajo.
async fn worker(sh: Arc<Shared>, net: Network) {
    sh.progress.connection_opened(net.id());
    let limiter = &sh.limits[&net.id()];
    let backoff = sh.opts.backoff();
    let mut failures = 0u32;
    loop {
        // Esperar hueco: si el servidor limita las conexiones, las de más
        // esperan aquí en lugar de insistir.
        if !limiter.acquire(sh.owner, &sh.abort).await {
            break;
        }
        sh.progress.set_limit(net.id(), limiter.allowed());
        let Some(lease) = sh.queue().next(net.id()) else {
            // Lo que falta ya tiene conexiones vivas; si alguna falla, ella
            // misma reintentará su bloque.
            limiter.release(sh.owner);
            break;
        };
        let (lease, res) = fetch(&sh, &net, lease).await;
        sh.queue().release(&lease);
        limiter.release(sh.owner);
        let Err(f) = res else {
            failures = 0;
            continue;
        };
        if f.progressed {
            failures = 0;
        }
        if matches!(f.error, Error::Cancelled) {
            break;
        }
        if !f.error.is_retriable() {
            sh.fail(f.error);
            break;
        }
        sh.progress.retried(net.id());

        // 429/503 con varias conexiones abiertas: el servidor limita cuántas
        // acepta. Se baja el límite y no cuenta como fallo.
        let too_many =
            matches!(f.error, Error::HttpStatus { status: 429 | 503 }) && limiter.reject();
        let delay = if too_many {
            sh.progress.set_limit(net.id(), limiter.allowed());
            info!(
                "{}: el servidor rechaza conexiones de más ({}); límite: {}",
                net.id(),
                f.error,
                limiter.allowed()
            );
            f.retry_after.unwrap_or(sh.opts.retry_base)
        } else {
            failures += 1;
            if failures > sh.opts.max_retries {
                sh.fail(Error::RetriesExhausted(Box::new(f.error)));
                break;
            }
            let delay = f.retry_after.unwrap_or_else(|| backoff.delay(failures - 1));
            info!(
                "{}: bloque en {} falló ({}); reintento en {delay:?}",
                net.id(),
                lease.start,
                f.error
            );
            delay
        };
        tokio::select! {
            _ = sh.abort.cancelled() => break,
            _ = tokio::time::sleep(delay.min(sh.opts.retry_max)) => {}
        }
    }
    sh.progress.connection_closed(net.id());
}

struct FetchError {
    error: Error,
    /// ¿Se llegó a escribir algo antes del fallo?
    progressed: bool,
    retry_after: Option<Duration>,
}

impl FetchError {
    fn new(error: Error, progressed: bool) -> Self {
        Self {
            error,
            progressed,
            retry_after: None,
        }
    }
}

/// Descarga el rango de una asignación y lo escribe en su sitio. Si la
/// petición abarca más bloques y el siguiente está libre, sigue con él sin
/// abrir otra petición. Devuelve la última asignación, para soltarla.
async fn fetch(sh: &Shared, net: &Network, lease: Lease) -> (Lease, Result<(), FetchError>) {
    let mut w = Writer {
        sh,
        net,
        lease,
        buf: Vec::with_capacity(sh.opts.write_buffer),
        pos: lease.start,
        progressed: false,
    };
    let res = fetch_inner(&mut w).await;
    (w.lease, res)
}

async fn fetch_inner(w: &mut Writer<'_>) -> Result<(), FetchError> {
    let (sh, net, lease) = (w.sh, w.net, w.lease);
    let mut req = net
        .client()
        .get(sh.info.url.clone())
        .headers(sh.opts.headers.clone())
        .header(
            RANGE,
            format!("bytes={}-{}", lease.start, lease.request_end - 1),
        );
    let if_range = sh.info.if_range();
    if let Some(v) = if_range {
        req = req.header(IF_RANGE, v);
    }
    let resp = tokio::select! {
        _ = sh.abort.cancelled() => return Err(FetchError::new(Error::Cancelled, false)),
        r = req.send() => r.map_err(|e| FetchError::new(e.into(), false))?,
    };

    match resp.status() {
        StatusCode::PARTIAL_CONTENT => {
            let cr = resp
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_content_range);
            match cr {
                Some(cr) if cr.total.is_some() && cr.total != sh.info.size => {
                    return Err(FetchError::new(Error::RemoteChanged, false));
                }
                Some(cr) if cr.start == lease.start => {}
                _ => return Err(FetchError::new(Error::RangeNotHonoured, false)),
            }
        }
        // Con If-Range, un 200 significa que el archivo cambió.
        StatusCode::OK if if_range.is_some() => {
            return Err(FetchError::new(Error::RemoteChanged, false));
        }
        StatusCode::OK => return Err(FetchError::new(Error::RangeNotHonoured, false)),
        s => {
            return Err(FetchError {
                error: status_error(s),
                progressed: false,
                retry_after: retry_after(resp.headers()),
            });
        }
    }

    sh.progress.receiving_started(net.id());
    let limiter = &*sh.limits[&net.id()];
    limiter.receiving_started();
    let _receiving = ReceivingGuard {
        progress: &sh.progress,
        limiter,
        network: net.id(),
    };
    let mut stream = resp.bytes_stream();
    let mut last_flush = Instant::now();
    loop {
        let deadline = last_flush + FLUSH_INTERVAL;
        let chunk = tokio::select! {
            biased;
            _ = sh.abort.cancelled() => {
                // Lo que ya llegó es válido: se escribe antes de salir.
                w.flush().await?;
                return Err(FetchError::new(Error::Cancelled, w.progressed));
            }
            _ = tokio::time::sleep_until(deadline), if !w.buf.is_empty() => {
                last_flush = Instant::now();
                if w.flush().await? == Commit::Stop {
                    return Ok(());
                }
                continue;
            }
            c = stream.next() => c,
        };
        match chunk {
            Some(Ok(bytes)) => {
                w.buf.extend_from_slice(&bytes);
                if !sh.opts.throttle(bytes.len() as u64, &sh.abort).await {
                    w.flush().await?;
                    return Err(FetchError::new(Error::Cancelled, w.progressed));
                }
                if w.buf.len() >= sh.opts.write_buffer || last_flush.elapsed() >= FLUSH_INTERVAL {
                    last_flush = Instant::now();
                    if w.flush().await? == Commit::Stop {
                        return Ok(());
                    }
                }
            }
            Some(Err(e)) => {
                w.flush().await?;
                return Err(FetchError::new(e.into(), w.progressed));
            }
            None => {
                if w.flush().await? == Commit::Stop {
                    return Ok(());
                }
                let end = sh.queue().allowed_end(&w.lease);
                return match end {
                    Some(end) if w.pos < end => Err(FetchError::new(
                        Error::ShortBody {
                            received: w.pos - w.lease.start,
                            expected: end - w.lease.start,
                        },
                        w.progressed,
                    )),
                    _ => Ok(()),
                };
            }
        }
    }
}

/// Marca una conexión como "recibiendo" mientras viva.
struct ReceivingGuard<'a> {
    progress: &'a Progress,
    limiter: &'a Limiter,
    network: crate::NetworkId,
}

impl Drop for ReceivingGuard<'_> {
    fn drop(&mut self) {
        self.progress.receiving_stopped(self.network);
        self.limiter.receiving_stopped();
    }
}

/// Búfer de escritura de una conexión.
struct Writer<'a> {
    sh: &'a Shared,
    net: &'a Network,
    /// Asignación actual (cambia al pasar al bloque siguiente).
    lease: Lease,
    buf: Vec<u8>,
    /// Siguiente posición del archivo a escribir.
    pos: u64,
    progressed: bool,
}

impl Writer<'_> {
    /// Escribe el búfer en su posición, recortado al final actual del bloque
    /// (puede haber encogido al partirse), y lo confirma en la cola. Si el
    /// bloque se termina y el siguiente está libre, sigue con él con lo que
    /// sobre del búfer.
    async fn flush(&mut self) -> Result<Commit, FetchError> {
        loop {
            let Some(end) = self.sh.queue().allowed_end(&self.lease) else {
                self.buf.clear();
                return Ok(Commit::Stop);
            };
            if self.buf.is_empty() {
                return Ok(Commit::Continue { end });
            }
            let n = (self.buf.len() as u64).min(end.saturating_sub(self.pos)) as usize;
            let mut data = std::mem::take(&mut self.buf);
            let rest = data.split_off(n);
            let file = self.sh.file.clone();
            let pos = self.pos;
            let mut data = tokio::task::spawn_blocking(move || {
                platform::write_all_at(&file, &data, pos).map(|()| data)
            })
            .await
            .map_err(|e| FetchError::new(Error::Io(io::Error::other(e)), self.progressed))?
            .map_err(|e| FetchError::new(Error::Io(e), self.progressed))?;
            self.pos += n as u64;
            self.progressed |= n > 0;
            let mut queue = self.sh.queue();
            let (gained, commit) = queue.commit(&self.lease, self.pos);
            self.sh.progress.add(self.net.id(), gained);
            // Bloque terminado: seguir con el siguiente con esta misma
            // petición si está libre (y si no hay que dejar sitio a otra
            // descarga al mismo servidor).
            if commit == Commit::Stop
                && self.pos == end
                && !self.sh.limits[&self.net.id()].should_yield(self.sh.owner)
                && let Some(next) = queue.extend(&self.lease, self.net.id())
            {
                self.lease = next;
                drop(queue);
                self.buf = rest;
                continue;
            }
            drop(queue);
            // Reutilizar la memoria del búfer.
            data.clear();
            data.extend_from_slice(&rest);
            self.buf = data;
            if commit == Commit::Stop {
                self.buf.clear();
            } else if self.sh.limits[&self.net.id()].should_yield(self.sh.owner) {
                // Otra descarga al mismo servidor espera hueco: se suelta
                // esta conexión y lo que falta del bloque queda libre.
                self.buf.clear();
                return Ok(Commit::Stop);
            }
            return Ok(commit);
        }
    }
}

/// Lo descargado a medias con una versión anterior tenía el estado junto al
/// archivo: se lleva a su sitio nuevo para poder reanudar.
fn adopt_legacy_state(dest: &Path, state: &Path) {
    let legacy = legacy_state_path_for(dest);
    if legacy == state || !legacy.exists() {
        return;
    }
    if !state.exists()
        && let Some(saved) = SavedState::load(&legacy)
        && let Err(e) = saved.save_atomic(state)
    {
        warn!("no se pudo mover el estado de {}: {e}", legacy.display());
        return;
    }
    let _ = fs::remove_file(&legacy);
    let _ = fs::remove_file(append_tmp(&legacy));
}

fn append_tmp(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".tmp");
    s.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::Block;

    #[test]
    fn legacy_state_is_adopted() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("viejo.bin");
        let legacy = legacy_state_path_for(&dest);
        let saved = SavedState::new(
            "http://x/viejo.bin".into(),
            10,
            None,
            None,
            vec![Block {
                start: 0,
                end: 10,
                written: 3,
            }],
        );
        saved.save_atomic(&legacy).unwrap();
        let state = dir.path().join("nuevo").join("estado.json");
        adopt_legacy_state(&dest, &state);
        assert!(!legacy.exists());
        assert_eq!(SavedState::load(&state).unwrap(), saved);
    }
}
