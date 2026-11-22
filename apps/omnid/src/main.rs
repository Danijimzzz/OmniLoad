//! `omnid`: el servicio de OmniLoad que descarga en segundo plano.
//!
//! Lo lanzan solos la ventana (`omniload`) y la CLI (`omni`) cuando hace
//! falta, así que normalmente no hay que arrancarlo a mano. Para depurar se
//! puede ejecutar en una terminal: escribe lo que hace y se para con Ctrl+C
//! (guardando el progreso). `OMNILOAD_DB` elige otra base de datos.
//!
//! La API y el formato de `omnid.json` están en el crate `omni-api`.

use std::process::ExitCode;

use omni_api::{Paths, Service, ServiceError};
use omni_core::DownloadOptions;
use tracing::{error, info};

fn main() -> ExitCode {
    init_logging();
    let Some(paths) = Paths::system() else {
        error!("no se encontró la carpeta de datos del sistema; usa OMNILOAD_DB");
        return ExitCode::FAILURE;
    };
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("omnid")
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!("no se pudo iniciar tokio: {e}");
            return ExitCode::FAILURE;
        }
    };
    let code = rt.block_on(async {
        let service = match Service::start(&paths, DownloadOptions::default()).await {
            Ok(s) => s,
            Err(ServiceError::AlreadyRunning) => {
                info!("ya hay un omnid en marcha; nada que hacer");
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                error!("no se pudo arrancar: {e}");
                return ExitCode::FAILURE;
            }
        };
        info!("base de datos: {}", paths.db.display());
        let stop = service.stop_requested();
        tokio::select! {
            _ = stop.cancelled() => {}
            r = tokio::signal::ctrl_c() => {
                if r.is_ok() {
                    info!("Ctrl+C: parando");
                }
            }
        }
        // Red de seguridad: si algo se atasca al parar (un hilo del sistema,
        // una conexión abierta), el proceso se termina igualmente a los 12 s
        // en vez de quedarse en el Administrador de tareas.
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(12));
            error!("el cierre tardó demasiado; se termina el proceso");
            std::process::exit(0);
        });
        service.stop().await;
        ExitCode::SUCCESS
    });
    // Sin esperar a tareas sueltas del runtime: el progreso ya está guardado.
    rt.shutdown_timeout(std::time::Duration::from_secs(1));
    // Y salir ya: en Windows un hilo suelto basta para dejar el proceso vivo.
    std::process::exit(if code == ExitCode::SUCCESS { 0 } else { 1 });
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
}
