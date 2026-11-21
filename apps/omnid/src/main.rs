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
    rt.block_on(async {
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
        service.stop().await;
        ExitCode::SUCCESS
    })
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
