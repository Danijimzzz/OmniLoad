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
mod popup;
mod settings;
mod views;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use omni_core::DownloadOptions;
use slint::winit_030::WinitWindowAccessor;
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
    /// El aviso de enlaces copiados, y su estado.
    popup: ClipPopup,
    pop: RefCell<popup::State>,
    pop_rows: Rc<VecModel<PopRow>>,
    pop_place: Cell<Option<(i32, i32)>>,
    next_batch: Cell<u64>,
}

impl App {
    fn ui(&self) -> Option<AppWindow> {
        self.ui.upgrade()
    }

    /// Dónde va cada paquete, según los ajustes.
    fn dest(&self) -> grabber::Dest {
        let s = self.settings.borrow();
        grabber::Dest {
            base: s.download_dir.clone(),
            per_package: s.package_folders,
            single_file_too: s.single_file_folders,
        }
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
        let grabbed = self.grabber.snapshot(&self.dest());
        let mut st = self.state.borrow_mut();
        views::fill_downloads(&ui, &self.dl_rows, &rows, &mut st);
        views::fill_grabber(&ui, &self.grab_rows, &grabbed, &mut st);
        drop(st);
        self.refresh_popup();

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
        self.analyze_in(text, quiet, 0);
    }

    fn analyze_in(&self, text: &str, quiet: bool, batch: u64) -> bool {
        let ok = match self.grabber.analyze(text, batch) {
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
                true
            }
            Err(msg) => {
                if !quiet {
                    self.show(&msg, true);
                }
                false
            }
        };
        self.refresh();
        ok
    }

    /// Texto copiado con enlaces: al capturador y, si está activado, al aviso
    /// de abajo a la derecha. Lo que se copie con el aviso abierto se suma a
    /// la misma tanda.
    fn copied(&self, text: &str) {
        let mode = self.settings.borrow().clip_popup;
        if mode == 2 {
            self.analyze(text, true);
            return;
        }
        let open = self.pop.borrow().is_open();
        if open {
            let batch = self.pop.borrow().batch;
            if self.analyze_in(text, true, batch) {
                self.pop.borrow_mut().more_links();
            }
            return;
        }
        let batch = self.next_batch.get() + 1;
        self.next_batch.set(batch);
        self.pop.borrow_mut().open(batch, mode == 1, Instant::now());
        if self.analyze_in(text, true, batch) {
            self.popup
                .set_autostart(self.settings.borrow().clip_popup_autostart);
            self.pop_place.set(None);
            self.refresh_popup();
            tracing::debug!("aviso: tanda {batch}");
            if let Err(e) = self.popup.show() {
                tracing::warn!("no se pudo abrir el aviso: {e}");
            }
            self.place_popup();
        } else {
            self.pop.borrow_mut().close();
        }
    }

    fn close_popup(&self) {
        tracing::debug!("aviso: cerrar");
        self.pop.borrow_mut().close();
        self.pop_rows.set_vec(Vec::new());
        let _ = self.popup.hide();
    }

    /// Redibuja el aviso y lo cierra solo si toca.
    fn refresh_popup(&self) {
        if !self.pop.borrow().is_open() {
            return;
        }
        let Some(ui) = self.ui() else { return };
        copy_theme(&ui, &self.popup);
        let batch = self.pop.borrow().batch;
        let items = self.grabber.batch_items(batch);
        let c = popup::counts(&items);
        if c.total == 0 && self.pop.borrow().should_autoclose(Instant::now()) {
            // Ya no queda nada de la tanda (se descargó o se quitó).
            self.close_popup();
            return;
        }
        let now = Instant::now();
        let time = self.pop.borrow_mut().tick(&c, now);
        if self.settings.borrow().clip_popup_autoclose
            && self.pop.borrow().should_autoclose(now)
            && !self.popup.get_hovered()
        {
            self.close_popup();
            return;
        }
        let rows = items
            .iter()
            .map(|i| {
                let (status, usable, tone) = popup::status(&i.check);
                PopRow {
                    key: i.id as i32,
                    name: i.name.clone().into(),
                    host: i.host.clone().into(),
                    size: i.size.map(format::bytes).unwrap_or_default().into(),
                    status: status.into(),
                    tone,
                    checked: usable && i.selected,
                    usable,
                }
            })
            .collect();
        sync_model(&self.pop_rows, rows);
        let (ok_arc, bad_arc) = popup::ring_arcs(c.ok, c.bad, c.total);
        let p = &self.popup;
        p.set_compact(self.pop.borrow().compact);
        p.set_total(c.total as i32);
        p.set_ok(c.ok as i32);
        p.set_bad(c.bad as i32);
        p.set_checking(c.checking > 0);
        p.set_time_text(time.into());
        p.set_ok_arc(ok_arc.into());
        p.set_bad_arc(bad_arc.into());
        p.set_to_download(c.chosen as i32);
        self.place_popup();
    }

    /// Lleva el aviso a la esquina de abajo a la derecha de la pantalla de la
    /// ventana principal.
    fn place_popup(&self) {
        let Some(ui) = self.ui() else { return };
        // El tamaño que pide ahora, no el de la ventana, que se pone al día
        // un poco después.
        let scale = self.popup.window().scale_factor();
        let want =
            slint::LogicalSize::new(self.popup.get_want_width(), self.popup.get_want_height());
        let size = want.to_physical(scale);
        if self.popup.window().size() != size {
            self.popup.window().set_size(want);
        }
        let monitor = ui
            .window()
            .with_winit_window(|w| w.current_monitor().or_else(|| w.primary_monitor()))
            .flatten();
        let Some(monitor) = monitor else { return };
        let pos = popup::corner(&monitor, size);
        if self.pop_place.get() != Some((pos.x, pos.y)) {
            self.popup.window().set_position(pos);
            self.pop_place.set(Some((pos.x, pos.y)));
        }
    }

    /// Pasa a la cola lo disponible y marcado de la tanda del aviso.
    fn popup_download(&self, start: bool) {
        let batch = self.pop.borrow().batch;
        let picked = self.grabber.take_batch(batch, &self.dest());
        self.close_popup();
        if !picked.is_empty() {
            self.queue(picked, start);
        }
    }

    /// Pasa lo marcado del capturador a la cola.
    fn grab_to_queue(&self, start: bool) {
        let picked = self.grabber.take_selected(&self.dest());
        if picked.is_empty() {
            self.show("No hay archivos marcados.", true);
            return;
        }
        self.queue(picked, start);
    }

    fn queue(&self, picked: Vec<grabber::Picked>, start: bool) {
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
        s.clip_popup = ui.get_clip_popup();
        s.clip_popup_autoclose = ui.get_clip_autoclose();
        s.clip_popup_autostart = self.popup.get_autostart();
        s.download_dir = PathBuf::from(ui.get_download_dir().as_str());
        s.package_folders = ui.get_package_folders();
        s.single_file_folders = ui.get_single_file_folders();
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

    popup::select_backend();
    let ui = AppWindow::new()?;
    apply_settings(&ui, &settings);
    let clip_popup = popup::create(ClipPopup::new)?;
    clip_popup.set_autostart(settings.clip_popup_autostart);
    let pop_rows: Rc<VecModel<PopRow>> = Rc::new(VecModel::default());
    clip_popup.set_rows(ModelRc::from(pop_rows.clone()));

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
        popup: clip_popup,
        pop: RefCell::default(),
        pop_rows,
        pop_place: Cell::new(None),
        next_batch: Cell::new(0),
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
                app.copied(&text);
            }
        });
    }

    connect_header(&ui, &app);
    connect_downloads(&ui, &app);
    connect_grabber(&ui, &app);
    connect_settings(&ui, &app);
    connect_popup(&ui, &app);

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

/// El aviso tiene su propia copia del tema: se iguala a la de la ventana.
fn copy_theme(ui: &AppWindow, popup: &ClipPopup) {
    let from = ui.global::<Theme>();
    let to = popup.global::<Theme>();
    to.set_kind(from.get_kind());
    to.set_follow_system(from.get_follow_system());
    to.set_system_dark(from.get_system_dark());
    to.set_accent_index(from.get_accent_index());
    to.set_compact(from.get_compact());
    to.set_reduce_transparency(from.get_reduce_transparency());
}

fn connect_popup(ui: &AppWindow, app: &Rc<App>) {
    let p = &app.popup;
    p.on_play({
        let app = app.clone();
        move || app.popup_download(true)
    });
    p.on_download({
        let app = app.clone();
        move || {
            let start = app.popup.get_autostart();
            app.popup_download(start);
        }
    });
    p.on_open_grabber({
        let app = app.clone();
        move || {
            app.close_popup();
            if let Some(ui) = app.ui() {
                ui.set_tab(1);
                ui.window().set_minimized(false);
                ui.window().with_winit_window(|w| w.focus_window());
            }
        }
    });
    p.on_discard({
        let app = app.clone();
        move || {
            let batch = app.pop.borrow().batch;
            app.grabber.discard_batch(batch);
            app.close_popup();
            app.refresh();
        }
    });
    p.on_close({
        let app = app.clone();
        move || app.close_popup()
    });
    p.on_minimize({
        let app = app.clone();
        move || {
            app.pop.borrow_mut().compact = true;
            app.refresh_popup();
        }
    });
    p.on_check({
        let app = app.clone();
        move |key, on| {
            app.grabber.set_item_selected(key as u64, on);
            app.refresh();
        }
    });
    p.on_autostart_changed({
        let app = app.clone();
        move || app.save_settings()
    });
    p.window().on_close_requested({
        let app = app.clone();
        move || {
            app.pop.borrow_mut().close();
            slint::CloseRequestResponse::HideWindow
        }
    });
    // Al cerrar la ventana principal se cierra también el aviso, para que el
    // programa termine.
    ui.window().on_close_requested({
        let app = app.clone();
        move || {
            app.close_popup();
            slint::CloseRequestResponse::HideWindow
        }
    });
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
    ui.set_clip_popup(s.clip_popup);
    ui.set_clip_autoclose(s.clip_popup_autoclose);
    ui.set_download_dir(s.download_dir.display().to_string().into());
    ui.set_package_folders(s.package_folders);
    ui.set_single_file_folders(s.single_file_folders);
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
