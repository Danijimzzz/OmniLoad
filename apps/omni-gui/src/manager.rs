//! La cola de la ventana: la del servicio `omnid`.
//!
//! Al abrir, la ventana busca el servicio; si no está en marcha lanza el
//! programa `omnid` que hay junto a `omniload`. Si no lo encuentra (por
//! ejemplo, con `cargo run -p omni-gui` sin haber compilado `omnid`), arranca
//! el mismo servicio dentro de la ventana: todo funciona igual, salvo que las
//! descargas se paran al cerrarla.
//!
//! La ventana siempre habla con la cola por la API ([`RemoteQueue`]), así que
//! hay un solo camino que probar.

use std::ops::Deref;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use omni_api::{Client, Paths, RemoteQueue, Service, ServiceError, connect_or_launch, omnid_exe};
use omni_core::DownloadOptions;
use tokio::runtime::Handle;
use tracing::{info, warn};

use crate::format;

pub use omni_queue::{JobInfo as RowInfo, Priority, State};

/// Cuánto esperar a que `omnid` conteste al lanzarlo.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Cuánto esperar a que guarde el progreso al cerrar.
const STOP_TIMEOUT: Duration = Duration::from_secs(8);

/// La cola, con el servicio dentro de la ventana si hizo falta arrancarlo
/// aquí.
pub struct Manager {
    remote: RemoteQueue,
    local: Mutex<Option<Service>>,
}

impl Deref for Manager {
    type Target = RemoteQueue;
    fn deref(&self) -> &RemoteQueue {
        &self.remote
    }
}

impl Manager {
    /// ¿Corre el servicio dentro de la ventana (y no en `omnid`)?
    pub fn in_process(&self) -> bool {
        self.local.lock().expect("servicio envenenado").is_some()
    }

    /// Al cerrar la ventana. Si `keep_running` y el servicio es `omnid`, las
    /// descargas siguen; si no, se paran guardando el progreso (y siguen
    /// solas la próxima vez).
    pub async fn close(&self, keep_running: bool) {
        let local = self.local.lock().expect("servicio envenenado").take();
        match local {
            Some(service) => {
                self.remote.close();
                service.stop().await;
            }
            None if keep_running => {
                info!("la ventana se cierra; omnid sigue descargando");
                self.remote.close();
            }
            None => self.remote.stop_service(STOP_TIMEOUT).await,
        }
    }
}

/// Encuentra o arranca el servicio y se conecta a él. Se llama desde el hilo
/// de la ventana, fuera del runtime.
pub fn open(rt: Handle) -> Manager {
    let paths = Paths::system().unwrap_or_else(temp_paths);
    let (client, local) = rt.block_on(connect(&paths)).unwrap_or_else(|e| {
        // Último recurso: una base de datos temporal, para que la ventana
        // funcione aunque no se guarde la lista.
        warn!("{e}; se usa una lista temporal");
        let paths = temp_paths();
        rt.block_on(async {
            let s = Service::start(&paths, DownloadOptions::default())
                .await
                .expect("servicio temporal");
            let c = Client::connect(&paths).await.expect("servicio temporal");
            (c, Some(s))
        })
    });
    Manager {
        remote: RemoteQueue::new(rt, client),
        local: Mutex::new(local),
    }
}

async fn connect(paths: &Paths) -> Result<(Client, Option<Service>), String> {
    let exe = omnid_exe();
    match connect_or_launch(paths, exe.as_deref(), LAUNCH_TIMEOUT).await {
        Ok(c) => return Ok((c, None)),
        Err(e) if exe.is_some() => warn!("{e}"),
        Err(_) => info!("no hay omnid junto a la ventana: el servicio corre dentro de ella"),
    }
    match Service::start(paths, DownloadOptions::default()).await {
        Ok(s) => {
            let c = Client::connect(paths)
                .await
                .ok_or("el servicio interno no contesta")?;
            Ok((c, Some(s)))
        }
        // Otro servicio tiene la base de datos pero aún no contesta (se
        // está arrancando): esperarlo.
        Err(ServiceError::AlreadyRunning) => {
            let deadline = Instant::now() + LAUNCH_TIMEOUT;
            while Instant::now() < deadline {
                if let Some(c) = Client::connect(paths).await {
                    return Ok((c, None));
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err("otro servicio de OmniLoad tiene la base de datos y no contesta".into())
        }
        Err(e) => Err(e.to_string()),
    }
}

fn temp_paths() -> Paths {
    Paths::for_db(
        std::env::temp_dir()
            .join(format!("omniload-{}", std::process::id()))
            .join("omniload.db"),
    )
}

/// Texto "a de b" para el tamaño.
pub fn sizes(r: &RowInfo) -> String {
    match r.total {
        Some(t) => format::bytes(t),
        None if r.downloaded > 0 => format::bytes(r.downloaded),
        None => String::new(),
    }
}
