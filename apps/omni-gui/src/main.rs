//! `omniload`: interfaz gráfica de OmniLoad, nativa con Slint.
//!
//! La ventana corre en el hilo principal y el motor (`omni-core`) en un
//! runtime de tokio dentro del mismo proceso. Un temporizador de Slint lee el
//! progreso cuatro veces por segundo y actualiza la lista.
//!
//! Como en JDownloader, los enlaces no se descargan directamente: primero van
//! al capturador ([`grabber`]), que enseña los archivos encontrados (también
//! los enlazados desde una página) y deja elegir cuáles bajar.
//!
//! `omniload <url>...` abre la ventana y analiza esos enlaces.
//!
//! Para ver los registros del motor, arranca desde una terminal con
//! `RUST_LOG=omni_core=debug`. En Windows, la versión release no abre consola;
//! la de depuración (`cargo run -p omni-gui`) sí.

// Sin ventana de consola en Windows (solo en release, para poder depurar).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod format;
mod grabber;
mod manager;
mod platform;

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use omni_core::DownloadOptions;
use slint::{ModelRc, SharedString, Timer, TimerMode, VecModel};

use grabber::{Check, Grabber};
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

    let grabber = Arc::new(Grabber::new(
        rt.handle().clone(),
        omni_core::Network::system_default()?.client().clone(),
    ));

    let rows: Rc<VecModel<DownloadRow>> = Rc::new(VecModel::default());
    ui.set_downloads(ModelRc::from(rows.clone()));
    let grab_rows: Rc<VecModel<GrabRow>> = Rc::new(VecModel::default());
    ui.set_grabbed(ModelRc::from(grab_rows.clone()));

    // Refresco de las dos listas.
    let refresh = {
        let manager = manager.clone();
        let grabber = grabber.clone();
        let ui_weak = ui.as_weak();
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            sync_model(&rows, manager.snapshot().into_iter().map(to_row).collect());
            let grabbed = grabber.snapshot();
            // Quita el "Analizando…" cuando ya se comprobó todo.
            if !ui.get_message_is_error()
                && ui.get_message().starts_with("Analizando")
                && !grabbed.iter().any(|r| r.check == Check::Checking)
            {
                show(&ui, "", false);
            }
            ui.set_grab_summary(grab_summary(&grabbed).into());
            ui.set_grab_selected(grabbed.iter().filter(|r| r.selected).count() as i32);
            sync_model(&grab_rows, grabbed.into_iter().map(to_grab_row).collect());
            let notes = grabber.take_notes();
            if !notes.is_empty() {
                show(&ui, &notes.join(" "), true);
            }
        }
    };
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(250), {
        let refresh = refresh.clone();
        move || refresh()
    });

    ui.on_analyze({
        let grabber = grabber.clone();
        let ui_weak = ui.as_weak();
        let refresh = refresh.clone();
        move |text| {
            let Some(ui) = ui_weak.upgrade() else { return };
            match grabber.analyze(&text) {
                Ok(n) => {
                    ui.set_links(SharedString::new());
                    ui.set_tab(0);
                    let msg = if n == 1 {
                        "Analizando 1 enlace…".to_owned()
                    } else {
                        format!("Analizando {n} enlaces…")
                    };
                    show(&ui, &msg, false);
                }
                Err(msg) => show(&ui, &msg, true),
            }
            refresh();
        }
    });

    ui.on_grab_toggle({
        let grabber = grabber.clone();
        let refresh = refresh.clone();
        move |i, on| {
            grabber.set_selected(i as usize, on);
            refresh();
        }
    });
    ui.on_grab_select_all({
        let grabber = grabber.clone();
        let refresh = refresh.clone();
        move |on| {
            grabber.select_all(on);
            refresh();
        }
    });
    ui.on_grab_remove({
        let grabber = grabber.clone();
        let refresh = refresh.clone();
        move |i| {
            grabber.remove(i as usize);
            refresh();
        }
    });
    ui.on_grab_clear({
        let grabber = grabber.clone();
        let refresh = refresh.clone();
        move || {
            grabber.clear();
            refresh();
        }
    });
    ui.on_grab_download({
        let grabber = grabber.clone();
        let manager = manager.clone();
        let ui_weak = ui.as_weak();
        let refresh = refresh.clone();
        move |folder, connections| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let folder = PathBuf::from(folder.trim());
            if folder.as_os_str().is_empty() {
                show(&ui, "Elige una carpeta de destino.", true);
                return;
            }
            let picked = grabber.take_selected();
            let mut errors = Vec::new();
            for p in &picked {
                if let Err(msg) = manager.add(
                    &p.url,
                    folder.clone(),
                    connections.max(1) as usize,
                    Some(p.name.clone()),
                ) {
                    errors.push(format!("{}: {msg}", p.name));
                }
            }
            if errors.is_empty() {
                show(&ui, "", false);
            } else {
                show(&ui, &errors.join(" "), true);
            }
            ui.set_tab(1);
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
    // "Abrir con" o un acceso directo: van al capturador, como los pegados.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        ui.invoke_analyze(args.join("\n").into());
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

/// Copia una foto al modelo de Slint, tocando solo las filas que cambiaron.
fn sync_model<T: Clone + PartialEq + 'static>(model: &VecModel<T>, new: Vec<T>) {
    use slint::Model;
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

fn grab_summary(rows: &[grabber::GrabRow]) -> String {
    let checking = rows.iter().filter(|r| r.check == Check::Checking).count();
    let selected: Vec<_> = rows.iter().filter(|r| r.selected).collect();
    let mut s = format!("{} archivos · {} seleccionados", rows.len(), selected.len());
    let known: u64 = selected.iter().filter_map(|r| r.bytes).sum();
    if known > 0 {
        s.push_str(&format!(" · {}", format::bytes(known)));
    }
    if checking > 0 {
        s.push_str(&format!(" · comprobando {checking}…"));
    }
    s
}

fn to_grab_row(r: grabber::GrabRow) -> GrabRow {
    GrabRow {
        name: r.name.into(),
        size: r.size.into(),
        source: r.source.into(),
        status: r.status.into(),
        state: match r.check {
            Check::Checking => GrabState::Checking,
            Check::Online { .. } => GrabState::Online,
            Check::Offline(_) => GrabState::Offline,
        },
        selected: r.selected,
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
