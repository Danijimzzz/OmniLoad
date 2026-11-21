//! Cliente para la ventana.
//!
//! La ventana pinta cuatro veces por segundo desde su hilo y no puede
//! quedarse esperando a la red. [`RemoteQueue`] pregunta al servicio en
//! segundo plano y guarda la última respuesta; las órdenes (pausar, quitar…)
//! se mandan sin esperar y piden una actualización en cuanto terminan. Tiene
//! los mismos métodos que [`omni_queue::Queue`], así la ventana apenas nota
//! la diferencia.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use omni_queue::{JobInfo, NewJob, Priority, QueueSettings};
use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::client::{Client, ClientError};

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
