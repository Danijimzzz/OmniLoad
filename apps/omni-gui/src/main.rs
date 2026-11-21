//! `omniload`: interfaz gráfica de OmniLoad, nativa con Slint.
//!
//! La ventana corre en el hilo principal y el motor (`omni-core`) en un
//! runtime de tokio dentro del mismo proceso. Un temporizador de Slint lee el
//! progreso cuatro veces por segundo y actualiza la lista.
//!
//! `omniload <url>...` abre la ventana y empieza a descargar esos enlaces en la
//! carpeta de descargas.
//!
//! Para ver los registros del motor, arranca desde una terminal con
//! `RUST_LOG=omni_core=debug`. En Windows, la versión release no abre consola;
//! la de depuración (`cargo run -p omni-gui`) sí.

// Sin ventana de consola en Windows (solo en release, para poder depurar).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod format;
mod manager;
mod platform;

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use omni_core::DownloadOptions;
use slint::{ModelRc, SharedString, Timer, TimerMode, VecModel};

use manager::{Manager, RowInfo, State};

slint::include_modules!();

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("omni-motor")
        .build()?;
    let manager = Arc::new(Manager::new(
        rt.handle().clone(),
        DownloadOptions::default(),
    ));

    let ui = AppWindow::new()?;
    ui.set_folder(default_folder().display().to_string().into());
    ui.set_connections(omni_core::DEFAULT_CONNECTIONS_PER_NETWORK as i32);

    let rows: Rc<VecModel<DownloadRow>> = Rc::new(VecModel::default());
    ui.set_downloads(ModelRc::from(rows.clone()));

    // Refresco de la lista.
    let refresh = {
        let manager = manager.clone();
        let rows = rows.clone();
        move || update_rows(&rows, manager.snapshot())
    };
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(250), {
        let refresh = refresh.clone();
        move || refresh()
    });

    ui.on_add_download({
        let manager = manager.clone();
        let ui_weak = ui.as_weak();
        let refresh = refresh.clone();
        move |url, folder, connections| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let folder = PathBuf::from(folder.trim());
            match manager.add(&url, folder, connections.max(1) as usize) {
                Ok(()) => {
                    ui.set_url(SharedString::new());
                    show(&ui, "", false);
                }
                Err(msg) => show(&ui, &msg, true),
            }
            refresh();
        }
    });

    ui.on_choose_folder({
        let ui_weak = ui.as_weak();
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let current = PathBuf::from(ui.get_folder().as_str());
            let mut dialog = rfd::FileDialog::new().set_title("Carpeta de descargas");
            if current.is_dir() {
                dialog = dialog.set_directory(&current);
            }
            if let Some(dir) = dialog.pick_folder() {
                ui.set_folder(dir.display().to_string().into());
            }
        }
    });

    ui.on_pause({
        let manager = manager.clone();
        let refresh = refresh.clone();
        move |i| {
            manager.pause(i as usize);
            refresh();
        }
    });
    ui.on_resume({
        let manager = manager.clone();
        let refresh = refresh.clone();
        move |i| {
            manager.resume(i as usize);
            refresh();
        }
    });
    ui.on_remove({
        let manager = manager.clone();
        let refresh = refresh.clone();
        move |i| {
            manager.remove(i as usize);
            refresh();
        }
    });
    ui.on_open_folder({
        let manager = manager.clone();
        let ui_weak = ui.as_weak();
        move |i| {
            let (Some(ui), Some(dir)) = (ui_weak.upgrade(), manager.folder(i as usize)) else {
                return;
            };
            if let Err(e) = platform::open_folder(&dir) {
                show(
                    &ui,
                    &format!("No se pudo abrir {}: {e}", dir.display()),
                    true,
                );
            }
        }
    });

    // Enlaces pasados como argumentos (`omniload <url>...`), por ejemplo desde
    // "Abrir con" o un acceso directo: se añaden con la carpeta por defecto.
    let folder = PathBuf::from(ui.get_folder().as_str());
    for url in std::env::args().skip(1) {
        if let Err(msg) = manager.add(&url, folder.clone(), ui.get_connections() as usize) {
            show(&ui, &format!("{url}: {msg}"), true);
        }
    }
    refresh();

    ui.run()?;

    // Al cerrar: pausar todo y dar tiempo al motor a guardar el progreso.
    drop(timer);
    if manager.any_active() {
        manager.shutdown(Duration::from_secs(5));
    }
    rt.shutdown_timeout(Duration::from_secs(1));
    Ok(())
}

fn show(ui: &AppWindow, msg: &str, error: bool) {
    ui.set_message(msg.into());
    ui.set_message_is_error(error);
}

/// Copia la foto del gestor al modelo de Slint, tocando solo lo que cambió.
fn update_rows(model: &VecModel<DownloadRow>, snapshot: Vec<RowInfo>) {
    use slint::Model;
    let new: Vec<DownloadRow> = snapshot.into_iter().map(to_row).collect();
    if model.row_count() != new.len() {
        model.set_vec(new);
        return;
    }
    for (i, row) in new.into_iter().enumerate() {
        if model.row_data(i).as_ref() != Some(&row) {
            model.set_row_data(i, row);
        }
    }
}

fn to_row(r: RowInfo) -> DownloadRow {
    DownloadRow {
        name: r.name.into(),
        folder: r.folder.into(),
        state: match r.state {
            State::Starting => RowState::Starting,
            State::Running => RowState::Running,
            State::Pausing => RowState::Pausing,
            State::Paused => RowState::Paused,
            State::Done => RowState::Done,
            State::Failed => RowState::Failed,
        },
        fraction: r.fraction.unwrap_or(-1.0),
        status: r.status.into(),
        sizes: r.sizes.into(),
        speed: r.speed.into(),
        eta: r.eta.into(),
        connections: r.connections.into(),
        detail: r.detail.into(),
    }
}

/// La carpeta de descargas del usuario, o la actual si no hay.
fn default_folder() -> PathBuf {
    dirs::download_dir()
        .or_else(dirs::home_dir)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}
