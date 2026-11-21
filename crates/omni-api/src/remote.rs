//! Cliente para la ventana.
//!
//! La ventana pinta cuatro veces por segundo desde su hilo y no puede
//! quedarse esperando a la red. [`RemoteQueue`] pregunta al servicio en
//! segundo plano y guarda la última respuesta; las órdenes (pausar, quitar…)
//! se mandan sin esperar y piden una actualización en cuanto terminan. Tiene
//! los mismos métodos que [`omni_queue::Queue`], así la ventana apenas nota
//! la diferencia.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use omni_grab::{Added, Check, Dest, GrabCommand, GrabItem, GrabPackage, Pick};
use omni_queue::{JobInfo, NewJob, Priority, QueueSettings};
use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::client::{Client, ClientError};
use crate::types::{AddLinksRequest, GrabberView, TakeRequest, TakeResponse};

/// Cada cuánto se pregunta al servicio.
const POLL: Duration = Duration::from_millis(250);

#[derive(Default)]
struct Cache {
    jobs: Vec<JobInfo>,
    settings: QueueSettings,
    /// ¿Contestó el servicio la última vez?
    online: bool,
}

/// La cola del servicio, vista desde la ventana.
pub struct RemoteQueue {
    client: Client,
    rt: Handle,
    cache: Arc<Mutex<Cache>>,
    poke: Arc<Notify>,
    stop: CancellationToken,
}

impl RemoteQueue {
    /// Empieza a seguir al servicio. Se llama fuera del runtime (desde el
    /// hilo de la ventana): espera a la primera respuesta para tener datos
    /// desde el principio.
    pub fn new(rt: Handle, client: Client) -> Self {
        let cache = Arc::new(Mutex::new(Cache::default()));
        rt.block_on(refresh(&client, &cache));
        let me = Self {
            client,
            rt,
            cache,
            poke: Arc::new(Notify::new()),
            stop: CancellationToken::new(),
        };
        me.rt.spawn(poller(
            me.client.clone(),
            me.cache.clone(),
            me.poke.clone(),
            me.stop.clone(),
        ));
        me
    }

    fn cache(&self) -> MutexGuard<'_, Cache> {
        self.cache.lock().expect("caché envenenada")
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// ¿Contesta el servicio?
    pub fn online(&self) -> bool {
        self.cache().online
    }

    /// Manda una orden sin esperar y pide una actualización al terminar.
    fn fire<F, Fut>(&self, f: F)
    where
        F: FnOnce(Client) -> Fut,
        Fut: Future<Output = Result<(), ClientError>> + Send + 'static,
    {
        let fut = f(self.client.clone());
        let poke = self.poke.clone();
        self.rt.spawn(async move {
            if let Err(e) = fut.await {
                warn!("orden al servicio fallida: {e}");
            }
            poke.notify_one();
        });
    }

    /// Añade una descarga. Espera la respuesta (como mucho unos segundos)
    /// para poder avisar de un enlace no válido.
    pub fn add(&self, new: NewJob) -> Result<u64, String> {
        let res = self.rt.block_on(self.client.add(&new));
        self.poke.notify_one();
        res.map_err(|e| e.to_string())
    }

    pub fn start(&self, ids: &[u64]) {
        let ids = ids.to_vec();
        self.fire(|c| async move { c.start(&ids).await });
    }

    pub fn stop(&self, ids: &[u64]) {
        let ids = ids.to_vec();
        self.fire(|c| async move { c.stop(&ids).await });
    }

    pub fn remove(&self, ids: &[u64]) {
        // Se quitan ya de la vista para que la lista no parpadee.
        self.cache().jobs.retain(|j| !ids.contains(&j.id));
        let ids = ids.to_vec();
        self.fire(|c| async move { c.remove(&ids).await });
    }

    pub fn set_priority(&self, ids: &[u64], priority: Priority) {
        let ids = ids.to_vec();
        self.fire(|c| async move { c.set_priority(&ids, priority).await });
    }

    pub fn set_job_speed_limit(&self, ids: &[u64], bytes_per_sec: Option<u64>) {
        let ids = ids.to_vec();
        self.fire(|c| async move { c.set_job_speed_limit(&ids, bytes_per_sec).await });
    }

    pub fn start_all(&self) {
        self.cache().settings.held = false;
        self.fire(|c| async move { c.start_all().await });
    }

    pub fn pause_all(&self) {
        self.cache().settings.held = true;
        self.fire(|c| async move { c.pause_all().await });
    }

    pub fn stop_all(&self) {
        self.cache().settings.held = false;
        self.fire(|c| async move { c.stop_all().await });
    }

    pub fn settings(&self) -> QueueSettings {
        self.cache().settings.clone()
    }

    /// Cambia ajustes de la cola partiendo de los últimos conocidos.
    pub fn update_settings(&self, f: impl FnOnce(&mut QueueSettings)) {
        let new = {
            let mut cache = self.cache();
            f(&mut cache.settings);
            cache.settings.clone()
        };
        if !self.online() {
            return;
        }
        self.fire(|c| async move { c.set_settings(&new).await.map(drop) });
    }

    pub fn is_held(&self) -> bool {
        self.cache().settings.held
    }

    pub fn package_ids(&self, package: &str) -> Vec<u64> {
        self.cache()
            .jobs
            .iter()
            .filter(|j| j.package == package)
            .map(|j| j.id)
            .collect()
    }

    pub fn any_active(&self) -> bool {
        self.cache().jobs.iter().any(|j| j.state.is_active())
    }

    /// La última foto de la lista.
    pub fn snapshot(&self) -> Vec<JobInfo> {
        self.cache().jobs.clone()
    }

    /// Deja de seguir al servicio.
    pub fn close(&self) {
        self.stop.cancel();
    }

    /// Pide al servicio que pare (guardando el progreso) y espera, como
    /// mucho `timeout`, a que deje de contestar.
    pub async fn stop_service(&self, timeout: Duration) {
        self.close();
        if self.client.shutdown().await.is_err() {
            return;
        }
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if self.client.status().await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

impl Drop for RemoteQueue {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn refresh(client: &Client, cache: &Mutex<Cache>) {
    let (jobs, settings) = tokio::join!(client.jobs(), client.settings());
    let mut c = cache.lock().expect("caché envenenada");
    match (jobs, settings) {
        (Ok(jobs), Ok(settings)) => {
            c.jobs = jobs;
            c.settings = settings;
            c.online = true;
        }
        (Err(e), _) | (_, Err(e)) => {
            if c.online {
                warn!("el servicio no contesta: {e}");
            }
            c.online = false;
        }
    }
}

async fn poller(
    client: Client,
    cache: Arc<Mutex<Cache>>,
    poke: Arc<Notify>,
    stop: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(POLL) => {}
            _ = poke.notified() => {}
        }
        refresh(&client, &cache).await;
    }
}

/// Cada cuánto se pregunta por el capturador.
const GRAB_POLL: Duration = Duration::from_millis(250);

#[derive(Default)]
struct GrabCache {
    view: GrabberView,
    /// Avisos aún no enseñados.
    notes: Vec<String>,
}

/// El capturador del servicio, visto desde la ventana. Como
/// [`RemoteQueue`]: pregunta en segundo plano, y los cambios se ven al
/// momento en la copia local mientras el servicio los aplica.
pub struct RemoteGrabber {
    client: Client,
    rt: Handle,
    cache: Arc<Mutex<GrabCache>>,
    /// Sube con cada orden: una respuesta pedida antes de la última orden
    /// está vieja y no se usa (si no, una casilla recién marcada parpadearía).
    seq: Arc<AtomicU64>,
    poke: Arc<Notify>,
    stop: CancellationToken,
}

impl RemoteGrabber {
    /// Empieza a seguir al capturador del servicio. Se llama fuera del
    /// runtime.
    pub fn new(rt: Handle, client: Client) -> Self {
        let me = Self {
            client,
            rt,
            cache: Arc::default(),
            seq: Arc::default(),
            poke: Arc::new(Notify::new()),
            stop: CancellationToken::new(),
        };
        me.rt.block_on(grab_refresh(&me.client, &me.cache, &me.seq));
        me.rt.spawn(grab_poller(
            me.client.clone(),
            me.cache.clone(),
            me.seq.clone(),
            me.poke.clone(),
            me.stop.clone(),
        ));
        me
    }

    fn cache(&self) -> MutexGuard<'_, GrabCache> {
        self.cache.lock().expect("caché envenenada")
    }

    /// Los paquetes del capturador.
    pub fn snapshot(&self) -> Vec<GrabPackage> {
        self.cache().view.packages.clone()
    }

    /// Nombres de los plugins de hosts del servicio.
    pub fn hosts(&self) -> Vec<String> {
        self.cache().view.hosts.clone()
    }

    /// Avisos pendientes; se vacían al leerlos.
    pub fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut self.cache().notes)
    }

    /// Los archivos de una tanda, el más reciente primero.
    pub fn batch_items(&self, batch: u64) -> Vec<GrabItem> {
        let mut items: Vec<GrabItem> = self
            .cache()
            .view
            .packages
            .iter()
            .flat_map(|p| &p.items)
            .filter(|i| i.batch == batch)
            .cloned()
            .collect();
        items.sort_by_key(|i| std::cmp::Reverse(i.id));
        items
    }

    /// Manda enlaces al capturador y espera la respuesta (para poder decir
    /// "no hay enlaces" o "ya estaban").
    pub fn add_links(&self, text: &str, new_batch: bool, batch: u64) -> Result<Added, String> {
        let req = AddLinksRequest {
            text: text.to_owned(),
            new_batch,
            batch,
        };
        let res = self.rt.block_on(self.client.add_links(&req));
        // Los enlaces nuevos se ven ya (el aviso los enseña al abrirse).
        self.rt
            .block_on(grab_refresh(&self.client, &self.cache, &self.seq));
        res.map_err(|e| e.to_string())
    }

    /// Manda una orden; la copia local cambia ya.
    pub fn command(&self, cmd: GrabCommand) {
        self.seq.fetch_add(1, Ordering::SeqCst);
        apply_local(&mut self.cache().view.packages, &cmd);
        let client = self.client.clone();
        let (poke, seq) = (self.poke.clone(), self.seq.clone());
        self.rt.spawn(async move {
            if let Err(e) = client.grab_command(&cmd).await {
                warn!("orden al capturador fallida: {e}");
            }
            seq.fetch_add(1, Ordering::SeqCst);
            poke.notify_one();
        });
    }

    /// Pasa archivos a la lista de descargas y espera el resultado.
    pub fn take(&self, pick: Pick, dest: Dest, start: bool) -> Result<TakeResponse, String> {
        self.seq.fetch_add(1, Ordering::SeqCst);
        let req = TakeRequest { pick, dest, start };
        let res = self.rt.block_on(self.client.grab_take(&req));
        self.rt
            .block_on(grab_refresh(&self.client, &self.cache, &self.seq));
        res.map_err(|e| e.to_string())
    }

    /// Deja de seguir al servicio.
    pub fn close(&self) {
        self.stop.cancel();
    }
}

impl Drop for RemoteGrabber {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// Lo que hará el servicio con una orden, hecho ya en la copia local.
fn apply_local(packages: &mut Vec<GrabPackage>, cmd: &GrabCommand) {
    let selectable = |i: &GrabItem| !matches!(i.check, Check::Offline(_));
    match cmd {
        GrabCommand::SelectItem { id, selected } => {
            for i in packages.iter_mut().flat_map(|p| &mut p.items) {
                if i.id == *id && selectable(i) {
                    i.selected = *selected;
                }
            }
        }
        GrabCommand::SelectPackage { id, selected } => {
            for p in packages.iter_mut().filter(|p| p.id == *id) {
                for i in &mut p.items {
                    i.selected = *selected && selectable(i);
                }
            }
        }
        GrabCommand::SelectAll { selected } => {
            for i in packages.iter_mut().flat_map(|p| &mut p.items) {
                i.selected = *selected && selectable(i);
            }
        }
        GrabCommand::RemovePackage { id } => packages.retain(|p| p.id != *id),
        GrabCommand::RemoveOffline => {
            for p in packages.iter_mut() {
                p.items.retain(selectable);
            }
        }
        GrabCommand::DiscardBatch { batch } => {
            for p in packages.iter_mut() {
                p.items.retain(|i| i.batch != *batch);
            }
        }
        GrabCommand::Clear => packages.clear(),
        GrabCommand::RenamePackage { id, name } => {
            if !name.trim().is_empty() {
                for p in packages.iter_mut().filter(|p| p.id == *id) {
                    p.name = name.trim().to_owned();
                }
            }
        }
        GrabCommand::SetPackageFolder { id, folder } => {
            for p in packages.iter_mut().filter(|p| p.id == *id) {
                p.custom_folder = (!folder.as_os_str().is_empty()).then(|| folder.clone());
            }
        }
        GrabCommand::SetPackagePriority { id, priority } => {
            for p in packages.iter_mut().filter(|p| p.id == *id) {
                p.priority = *priority;
            }
        }
        GrabCommand::Recheck { ids } => {
            for i in packages.iter_mut().flat_map(|p| &mut p.items) {
                if (ids.is_empty() && !selectable(i)) || ids.contains(&i.id) {
                    i.check = Check::Checking;
                }
            }
        }
    }
    packages.retain(|p| !p.items.is_empty());
}

async fn grab_refresh(client: &Client, cache: &Mutex<GrabCache>, seq: &AtomicU64) {
    let before = seq.load(Ordering::SeqCst);
    match client.grabber().await {
        Ok(mut view) => {
            let mut c = cache.lock().expect("caché envenenada");
            c.notes.append(&mut view.notes);
            if seq.load(Ordering::SeqCst) == before {
                c.view = view;
            }
        }
        Err(e) => tracing::debug!("el capturador no contesta: {e}"),
    }
}

async fn grab_poller(
    client: Client,
    cache: Arc<Mutex<GrabCache>>,
    seq: Arc<AtomicU64>,
    poke: Arc<Notify>,
    stop: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(GRAB_POLL) => {}
            _ = poke.notified() => {}
        }
        grab_refresh(&client, &cache, &seq).await;
    }
}
