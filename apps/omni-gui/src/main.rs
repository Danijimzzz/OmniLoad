//! `omniload`: interfaz gráfica de OmniLoad, nativa con Slint.
//!
//! La ventana corre en el hilo principal y el motor (`omni-core`) en un
//! runtime de tokio dentro del mismo proceso. Un temporizador de Slint lee el
//! estado cuatro veces por segundo y actualiza las tablas.
//!
//! Como en JDownloader, los enlaces no se descargan directamente: primero van
//! al capturador ([`grabber`]), que los agrupa en paquetes y enseña qué hay
//! antes de bajar nada. De ahí pasan a la cola ([`manager`]).
//!
//! `omniload <url>...` abre la ventana y analiza esos enlaces.
//!
//! Para ver los registros del motor, arranca desde una terminal con
//! `RUST_LOG=omni_core=debug`. En Windows, la versión release no abre consola;
//! la de depuración (`cargo run -p omni-gui`) sí.

// Sin ventana de consola en Windows (solo en release, para poder depurar).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod clipboard;
mod format;
mod grabber;
mod manager;
mod platform;
mod settings;
mod views;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use omni_core::DownloadOptions;
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

use grabber::Grabber;
use manager::{Manager, NewDownload, Priority};
use settings::Settings;
use views::{DlSelection, UiState};

slint::include_modules!();

/// Todo lo que comparten los callbacks de la ventana.
struct App {
    ui: slint::Weak<AppWindow>,
    manager: Arc<Manager>,
    grabber: Arc<Grabber>,
    settings: RefCell<Settings>,
    state: RefCell<UiState>,
    dl_rows: Rc<VecModel<DlRow>>,
    grab_rows: Rc<VecModel<GrabTreeRow>>,
}

impl App {
    fn ui(&self) -> Option<AppWindow> {
        self.ui.upgrade()
    }

    fn base_dir(&self) -> PathBuf {
        self.settings.borrow().download_dir.clone()
    }

    fn show(&self, msg: &str, error: bool) {
        if let Some(ui) = self.ui() {
            ui.set_message(msg.into());
            ui.set_message_is_error(error);
        }
    }

    /// Redibuja todo a partir del estado del gestor y del capturador.
    fn refresh(&self) {
        let Some(ui) = self.ui() else { return };
        let rows = self.manager.snapshot();
        let grabbed = self.grabber.snapshot(&self.base_dir());
        let mut st = self.state.borrow_mut();
        views::fill_downloads(&ui, &self.dl_rows, &rows, &mut st);
        views::fill_grabber(&ui, &self.grab_rows, &grabbed, &mut st);
        drop(st);

        let notes = self.grabber.take_notes();
        if !notes.is_empty() {
            self.show(&notes.join(" "), true);
        } else if !ui.get_message_is_error()
            && ui.get_message().starts_with("Analizando")
            && !grabbed
                .iter()
                .flat_map(|p| &p.items)
                .any(|i| i.check == grabber::Check::Checking)
        {
            self.show("", false);
        }
    }

    fn analyze(&self, text: &str, quiet: bool) {
        match self.grabber.analyze(text) {
            Ok(n) => {
                let msg = if n == 1 {
                    "Analizando 1 enlace…".to_owned()
                } else {
                    format!("Analizando {n} enlaces…")
                };
                self.show(&msg, false);
                if !quiet && let Some(ui) = self.ui() {
                    ui.set_tab(1);
                }
            }
            Err(msg) if !quiet => self.show(&msg, true),
            Err(_) => {}
        }
        self.refresh();
    }

    /// Pasa lo marcado del capturador a la cola.
    fn grab_to_queue(&self, start: bool) {
        let picked = self.grabber.take_selected(&self.base_dir());
        if picked.is_empty() {
            self.show("No hay archivos marcados.", true);
            return;
        }
        let mut errors = Vec::new();
        for p in picked {
            let new = NewDownload {
                url: p.url,
                dir: p.folder,
                name: Some(p.name.clone()),
                package: p.package,
                priority: p.priority,
                size: p.size,
                start,
            };
            if let Err(msg) = self.manager.add(new) {
                errors.push(format!("{}: {msg}.", p.name));
            }
        }
        if errors.is_empty() {
            self.show("", false);
        } else {
            self.show(&errors.join(" "), true);
        }
        if start {
            self.manager.start_all();
        }
        if let Some(ui) = self.ui() {
            ui.set_tab(0);
        }
        self.refresh();
    }

    /// Ids de las descargas que afecta la selección actual.
    fn selected_ids(&self) -> Vec<u64> {
        match &self.state.borrow().dl_selected {
            Some(DlSelection::Package(name)) => self.manager.package_ids(name),
            Some(DlSelection::File(id)) => vec![*id],
            None => Vec::new(),
        }
    }

    fn save_settings(&self) {
        let Some(ui) = self.ui() else { return };
        let theme = ui.global::<Theme>();
        let mut s = self.settings.borrow_mut();
        s.theme = theme.get_kind();
        s.follow_system = theme.get_follow_system();
        s.accent = theme.get_accent_index();
        s.compact = theme.get_compact();
        s.reduce_transparency = theme.get_reduce_transparency();
        s.properties_panel = ui.get_properties_panel();
        s.max_active = ui.get_max_active();
        s.connections = ui.get_connections();
        s.watch_clipboard = ui.get_watch_clipboard();
        s.download_dir = PathBuf::from(ui.get_download_dir().as_str());
        s.save();
        self.manager.set_max_active(s.max_active as usize);
        self.manager.set_connections(s.connections as usize);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("omni-motor")
        .build()?;
    let settings = Settings::load();
    let manager = Arc::new(Manager::new(
        rt.handle().clone(),
        DownloadOptions::default(),
    ));
    manager.set_max_active(settings.max_active as usize);
    manager.set_connections(settings.connections as usize);
    let grabber = Arc::new(Grabber::new(
        rt.handle().clone(),
        omni_core::Network::system_default()?.client().clone(),
    ));

    let ui = AppWindow::new()?;
    apply_settings(&ui, &settings);

    let dl_rows: Rc<VecModel<DlRow>> = Rc::new(VecModel::default());
    ui.set_downloads(ModelRc::from(dl_rows.clone()));
    let grab_rows: Rc<VecModel<GrabTreeRow>> = Rc::new(VecModel::default());
    ui.set_grabbed(ModelRc::from(grab_rows.clone()));

    let app = Rc::new(App {
        ui: ui.as_weak(),
        manager: manager.clone(),
        grabber,
        settings: RefCell::new(settings),
        state: RefCell::new(UiState::default()),
        dl_rows,
        grab_rows,
    });

    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(250), {
        let app = app.clone();
        move || app.refresh()
    });

    // Vigilar el portapapeles, como JDownloader: lo que se copie con enlaces
    // va al capturador.
    let clip_timer = Timer::default();
    {
        let app = app.clone();
        let mut watcher = clipboard::Watcher::new();
        clip_timer.start(TimerMode::Repeated, Duration::from_secs(1), move || {
            let on = app.ui().is_some_and(|ui| ui.get_watch_clipboard());
            if let Some(text) = watcher.poll(on) {
                app.analyze(&text, true);
            }
        });
    }

    connect_header(&ui, &app);
    connect_downloads(&ui, &app);
    connect_grabber(&ui, &app);
    connect_settings(&ui, &app);

    // Enlaces pasados como argumentos (`omniload <url>...`), por ejemplo desde
    // "Abrir con" o un acceso directo: van al capturador, como los pegados.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        app.analyze(&args.join("\n"), false);
    }
    app.refresh();

    ui.run()?;

    // Al cerrar: pausar todo y dar tiempo al motor a guardar el progreso.
    drop(timer);
    drop(clip_timer);
    app.save_settings();
    if manager.any_active() {
        manager.shutdown(Duration::from_secs(5));
    }
    rt.shutdown_timeout(Duration::from_secs(1));
    Ok(())
}

fn apply_settings(ui: &AppWindow, s: &Settings) {
    let theme = ui.global::<Theme>();
    theme.set_kind(s.theme);
    theme.set_follow_system(s.follow_system);
    theme.set_accent_index(s.accent);
    theme.set_compact(s.compact);
    theme.set_reduce_transparency(s.reduce_transparency);
    ui.set_properties_panel(s.properties_panel);
    ui.set_max_active(s.max_active);
    ui.set_connections(s.connections);
    ui.set_watch_clipboard(s.watch_clipboard);
    ui.set_download_dir(s.download_dir.display().to_string().into());
}

/// Elige carpeta con el diálogo nativo del sistema.
fn pick_folder(current: &str) -> Option<PathBuf> {
    let current = PathBuf::from(current);
    let mut dialog = rfd::FileDialog::new().set_title("Elegir carpeta");
    if current.is_dir() {
        dialog = dialog.set_directory(&current);
    }
    dialog.pick_folder()
}

fn connect_header(ui: &AppWindow, app: &Rc<App>) {
    ui.on_add_links({
        let app = app.clone();
        move |text| {
            app.analyze(&text, false);
            if let Some(ui) = app.ui() {
                ui.set_add_dialog_open(false);
                ui.set_add_dialog_text(SharedString::new());
            }
        }
    });
}

fn connect_downloads(ui: &AppWindow, app: &Rc<App>) {
    ui.on_dl_click({
        let app = app.clone();
        move |key, is_package| {
            app.state.borrow_mut().select_download(key, is_package);
            app.refresh();
        }
    });
    ui.on_dl_toggle({
        let app = app.clone();
        move |key| {
            app.state.borrow_mut().toggle_download_package(key);
            app.refresh();
        }
    });
    ui.on_start_all({
        let app = app.clone();
        move || {
            app.manager.start_all();
            app.refresh();
        }
    });
    ui.on_pause_all({
        let app = app.clone();
        move || {
            app.manager.pause_all();
            app.refresh();
        }
    });
    ui.on_stop_all({
        let app = app.clone();
        move || {
            app.manager.stop_all();
            app.refresh();
        }
    });
    ui.on_queue_settings_changed({
        let app = app.clone();
        move || app.save_settings()
    });
    ui.on_dl_search({
        let app = app.clone();
        move |text| {
            app.state.borrow_mut().dl_search = text.to_string();
            app.refresh();
        }
    });
    ui.on_sel_start({
        let app = app.clone();
        move || {
            app.manager.start(&app.selected_ids());
            app.refresh();
        }
    });
    ui.on_sel_stop({
        let app = app.clone();
        move || {
            app.manager.stop(&app.selected_ids());
            app.refresh();
        }
    });
    ui.on_sel_remove({
        let app = app.clone();
        move || {
            app.manager.remove(&app.selected_ids());
            app.state.borrow_mut().dl_selected = None;
            app.refresh();
        }
    });
    ui.on_change_sel_priority({
        let app = app.clone();
        move |i| {
            app.manager
                .set_priority(&app.selected_ids(), Priority::from_index(i as usize));
            app.refresh();
        }
    });
    ui.on_sel_open_folder({
        let app = app.clone();
        move || {
            let folder = app.state.borrow().dl_selected_folder.clone();
            if let Some(dir) = folder
                && let Err(e) = platform::open_folder(&dir)
            {
                app.show(&format!("No se pudo abrir {}: {e}", dir.display()), true);
            }
        }
    });
}

fn connect_grabber(ui: &AppWindow, app: &Rc<App>) {
    ui.on_grab_click({
        let app = app.clone();
        move |key, is_package| {
            app.state.borrow_mut().select_grab(key, is_package);
            app.refresh();
        }
    });
    ui.on_grab_toggle({
        let app = app.clone();
        move |key| {
            app.state.borrow_mut().toggle_grab_package(key);
            app.refresh();
        }
    });
    ui.on_grab_check({
        let app = app.clone();
        move |key, is_package, on| {
            if is_package {
                app.grabber.set_package_selected(key as u64, on);
            } else {
                app.grabber.set_item_selected(key as u64, on);
            }
            app.refresh();
        }
    });
    ui.on_grab_select_all({
        let app = app.clone();
        move |on| {
            app.grabber.select_all(on);
            app.refresh();
        }
    });
    ui.on_grab_remove_offline({
        let app = app.clone();
        move || {
            app.grabber.remove_offline();
            app.refresh();
        }
    });
    ui.on_grab_clear({
        let app = app.clone();
        move || {
            app.grabber.clear();
            app.refresh();
        }
    });
    ui.on_grab_search({
        let app = app.clone();
        move |text| {
            app.state.borrow_mut().grab_search = text.to_string();
            app.refresh();
        }
    });
    ui.on_grab_to_downloads({
        let app = app.clone();
        move |start| app.grab_to_queue(start)
    });
    ui.on_pkg_rename({
        let app = app.clone();
        move |name| {
            if let Some(id) = app.state.borrow().grab_package() {
                app.grabber.rename_package(id, &name);
            }
            app.refresh();
        }
    });
    ui.on_pkg_folder({
        let app = app.clone();
        move |folder| {
            if let Some(id) = app.state.borrow().grab_package() {
                app.grabber
                    .set_package_folder(id, PathBuf::from(folder.trim()));
            }
            app.refresh();
        }
    });
    ui.on_pkg_choose_folder({
        let app = app.clone();
        move || {
            let Some(ui) = app.ui() else { return };
            let id = app.state.borrow().grab_package();
            if let (Some(id), Some(dir)) = (id, pick_folder(&ui.get_pkg_folder_text())) {
                app.grabber.set_package_folder(id, dir);
                app.refresh();
            }
        }
    });
    ui.on_change_pkg_priority({
        let app = app.clone();
        move |i| {
            if let Some(id) = app.state.borrow().grab_package() {
                app.grabber
                    .set_package_priority(id, Priority::from_index(i as usize));
            }
            app.refresh();
        }
    });
    ui.on_pkg_remove({
        let app = app.clone();
        move || {
            let id = app.state.borrow().grab_package();
            if let Some(id) = id {
                app.grabber.remove_package(id);
            }
            app.refresh();
        }
    });
}

fn connect_settings(ui: &AppWindow, app: &Rc<App>) {
    ui.on_settings_changed({
        let app = app.clone();
        move || app.save_settings()
    });
    ui.on_choose_download_dir({
        let app = app.clone();
        move || {
            let Some(ui) = app.ui() else { return };
            if let Some(dir) = pick_folder(&ui.get_download_dir()) {
                ui.set_download_dir(dir.display().to_string().into());
                app.save_settings();
                app.refresh();
            }
        }
    });
}

/// Copia una foto al modelo de Slint, tocando solo las filas que cambiaron.
fn sync_model<T: Clone + PartialEq + 'static>(model: &VecModel<T>, new: Vec<T>) {
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

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}
