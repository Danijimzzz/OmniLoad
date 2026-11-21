//! `omniload`: interfaz gráfica de OmniLoad, nativa con Slint.
//!
//! La ventana corre en el hilo principal. Las descargas las hace el servicio
//! `omnid` (ver [`manager`]), que guarda la lista en SQLite: al volver a abrir
//! todo sigue donde estaba, y con "Seguir descargando al salir"
//! las descargas continúan con la ventana cerrada. Un temporizador de Slint
//! lee el estado cuatro veces por segundo y actualiza las tablas.
//!
//! Como en JDownloader, los enlaces no se descargan directamente: primero van
//! al capturador, que los agrupa en paquetes y enseña qué hay antes de bajar
//! nada. El capturador también vive en el servicio ([`omni_grab`]); la
//! ventana lo ve con [`RemoteGrabber`]. De ahí pasan a la cola ([`manager`]).
//!
//! `omniload <url o archivo>...` abre la ventana y analiza esos enlaces (o
//! los de esos archivos `.txt`/`.html`).
//!
//! Con icono en la bandeja del sistema ([`tray`]), cerrar o minimizar la
//! ventana la esconde allí; «Salir» en el menú del icono cierra de verdad.
//! Solo hay una ventana a la vez ([`single`]): abrir OmniLoad otra vez enseña
//! la que ya estaba.
//!
//! Para ver los registros del motor, arranca desde una terminal con
//! `RUST_LOG=omni_core=debug`. En Windows, la versión release no abre consola;
//! la de depuración (`cargo run -p omni-gui`) sí.

// Sin ventana de consola en Windows (solo en release, para poder depurar).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod clipboard;
mod format;
mod manager;
mod notify;
mod platform;
mod popup;
mod settings;
mod single;
mod tray;
mod views;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use slint::winit_030::WinitWindowAccessor;
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

use manager::{Manager, Priority};
use omni_api::{RemoteGrabber, TakeResponse};
use omni_grab::{Check, Dest, GrabCommand, Pick};
use settings::Settings;
use tray::{Tray, TrayEvent, TrayStatus};
use views::{DlSelection, UiState};

slint::include_modules!();

/// Todo lo que comparten los callbacks de la ventana.
struct App {
    ui: slint::Weak<AppWindow>,
    manager: Arc<Manager>,
    grabber: RemoteGrabber,
    settings: RefCell<Settings>,
    state: RefCell<UiState>,
    dl_rows: Rc<VecModel<DlRow>>,
    grab_rows: Rc<VecModel<GrabTreeRow>>,
    /// El aviso de enlaces copiados, y su estado.
    popup: ClipPopup,
    pop: RefCell<popup::State>,
    pop_rows: Rc<VecModel<PopRow>>,
    pop_place: Cell<Option<(i32, i32)>>,
    rt: tokio::runtime::Handle,
    /// Icono de la bandeja (si el sistema tiene bandeja).
    tray: RefCell<Option<Tray>>,
    tray_tx: Sender<TrayEvent>,
    tray_rx: Receiver<TrayEvent>,
    /// Argumentos de otras ventanas que se abrieron estando esta en marcha.
    instance_rx: Receiver<Vec<String>>,
    /// La ventana está escondida en la bandeja.
    hidden: Cell<bool>,
    notes: RefCell<notify::Tracker>,
}

impl App {
    fn ui(&self) -> Option<AppWindow> {
        self.ui.upgrade()
    }

    /// Dónde va cada paquete, según los ajustes.
    fn dest(&self) -> Dest {
        let s = self.settings.borrow();
        Dest {
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
        self.notify_finished(&ui, &rows);
        self.update_tray(&ui, &rows);
        if self.hidden.get() {
            // Escondida en la bandeja: no hace falta pintar las tablas (con
            // miles de filas se notaría), pero el aviso de enlaces sigue.
            self.refresh_popup();
            return;
        }
        let grabbed = self.grabber.snapshot();
        let dest = self.dest();
        let mut st = self.state.borrow_mut();
        views::fill_downloads(&ui, &self.dl_rows, &rows, &mut st);
        views::fill_grabber(&ui, &self.grab_rows, &grabbed, &dest, &mut st);
        drop(st);
        sync_queue_settings(&ui, &self.manager);
        self.refresh_popup();

        let notes = self.grabber.take_notes();
        if !notes.is_empty() {
            self.show(&notes.join(" "), true);
        } else if !ui.get_message_is_error()
            && ui.get_message().starts_with("Analizando")
            && !grabbed
                .iter()
                .flat_map(|p| &p.items)
                .any(|i| i.check == Check::Checking)
        {
            self.show("", false);
        }
    }

    /// Avisa con una notificación del sistema de lo que acaba de terminar,
    /// si la ventana no está a la vista.
    fn notify_finished(&self, ui: &AppWindow, rows: &[manager::RowInfo]) {
        let notes = self.notes.borrow_mut().update(rows);
        if notes.is_empty() || !self.settings.borrow().notify_done {
            return;
        }
        let in_view = !self.hidden.get()
            && !ui.window().is_minimized()
            && ui
                .window()
                .with_winit_window(|w| w.has_focus())
                .unwrap_or(false);
        if in_view {
            return;
        }
        for n in notes {
            notify::send(&self.rt, n);
        }
    }

    /// Pone al día la ayuda y el menú del icono de la bandeja.
    fn update_tray(&self, ui: &AppWindow, rows: &[manager::RowInfo]) {
        let guard = self.tray.borrow();
        let Some(tray) = guard.as_ref() else { return };
        let active = rows.iter().filter(|r| r.state.is_active()).count();
        let waiting = rows
            .iter()
            .filter(|r| r.state == manager::State::Queued)
            .count();
        let rate = rows.iter().map(|r| r.rate).sum();
        let paused = self.manager.is_held();
        tray.update(&TrayStatus {
            tooltip: tray::tooltip(active, waiting, rate, paused),
            paused,
            clipboard: ui.get_watch_clipboard(),
        });
        // Si la bandeja desaparece (se cerró el panel del escritorio) con la
        // ventana escondida, la ventana vuelve: si no, no habría forma de
        // llegar a ella.
        let lost = !tray.available();
        drop(guard);
        if self.hidden.get() && lost {
            self.show_window();
        }
    }

    /// ¿Hay un icono en la bandeja donde esconder la ventana?
    fn tray_available(&self) -> bool {
        self.tray.borrow().as_ref().is_some_and(Tray::available)
    }

    /// Esconde la ventana en la bandeja. Las descargas siguen.
    fn hide_to_tray(&self) {
        let Some(ui) = self.ui() else { return };
        self.hidden.set(true);
        let _ = ui.hide();
        let first = !self.settings.borrow().tray_hint_shown;
        if first {
            self.settings.borrow_mut().tray_hint_shown = true;
            self.settings.borrow().save();
            notify::send(
                &self.rt,
                notify::Note {
                    title: "OmniLoad sigue en la bandeja".into(),
                    body: "Las descargas continúan. Para salir del todo, usa «Salir» en el menú del icono.".into(),
                },
            );
        }
    }

    /// Saca la ventana de la bandeja (o la trae delante).
    fn show_window(&self) {
        let Some(ui) = self.ui() else { return };
        let was_hidden = self.hidden.replace(false);
        if let Err(e) = ui.show() {
            tracing::warn!("no se pudo enseñar la ventana: {e}");
        }
        ui.window().set_minimized(false);
        ui.window().with_winit_window(|w| w.focus_window());
        if was_hidden {
            self.refresh();
        }
    }

    /// Lo pedido desde el icono de la bandeja o desde otra ventana.
    fn handle_events(&self) {
        while let Ok(ev) = self.tray_rx.try_recv() {
            match ev {
                TrayEvent::Show => self.show_window(),
                TrayEvent::StartAll => self.manager.start_all(),
                TrayEvent::PauseAll => self.manager.pause_all(),
                TrayEvent::ToggleClipboard => {
                    if let Some(ui) = self.ui() {
                        ui.set_watch_clipboard(!ui.get_watch_clipboard());
                        self.save_settings();
                    }
                }
                TrayEvent::Quit => {
                    let _ = slint::quit_event_loop();
                }
            }
        }
        while let Ok(args) = self.instance_rx.try_recv() {
            self.show_window();
            self.open_args(args);
        }
        // Minimizar = esconder en la bandeja, si así se eligió.
        if !self.hidden.get()
            && self.settings.borrow().minimize_to_tray
            && self.tray_available()
            && self.ui().is_some_and(|ui| ui.window().is_minimized())
        {
            self.hide_to_tray();
        }
    }

    /// El botón de cerrar de la ventana.
    fn close_requested(&self) {
        if self.settings.borrow().close_to_tray && self.tray_available() {
            self.hide_to_tray();
        } else {
            self.close_popup();
            let _ = slint::quit_event_loop();
        }
    }

    /// Pone el icono en la bandeja. Se llama con el bucle de eventos ya en
    /// marcha (macOS lo exige).
    fn create_tray(&self) {
        match Tray::new(&self.rt, self.tray_tx.clone()) {
            Ok(t) => *self.tray.borrow_mut() = Some(t),
            Err(e) => tracing::info!("sin icono en la bandeja ({e}): cerrar la ventana sale"),
        }
        if let Some(ui) = self.ui() {
            ui.set_tray_available(self.tray.borrow().is_some());
        }
        self.refresh();
    }

    /// Enlaces y archivos pasados como argumentos (`omniload <url>...`), por
    /// ejemplo desde «Abrir con» o un acceso directo: van al capturador, como
    /// los pegados.
    fn open_args(&self, args: Vec<String>) {
        let (files, links): (Vec<String>, Vec<String>) = args
            .into_iter()
            .partition(|a| std::path::Path::new(a).is_file());
        if !links.is_empty() {
            self.analyze(&links.join("\n"), false);
        }
        if !files.is_empty() {
            let files: Vec<PathBuf> = files.into_iter().map(PathBuf::from).collect();
            self.analyze_files(&files);
        }
    }

    fn analyze(&self, text: &str, quiet: bool) {
        self.analyze_in(text, quiet, false, 0);
    }

    /// Manda enlaces al capturador. Con `new_batch` forman una tanda nueva;
    /// si no, se suman a `batch` (0: ninguna). Devuelve la tanda, o `None`
    /// si no había enlaces nuevos.
    fn analyze_in(&self, text: &str, quiet: bool, new_batch: bool, batch: u64) -> Option<u64> {
        let res = match self.grabber.add_links(text, new_batch, batch) {
            Ok(added) => {
                let n = added.added;
                let msg = if n == 1 {
                    "Analizando 1 enlace…".to_owned()
                } else {
                    format!("Analizando {n} enlaces…")
                };
                self.show(&msg, false);
                if !quiet && let Some(ui) = self.ui() {
                    ui.set_tab(1);
                }
                Some(added.batch)
            }
            Err(msg) => {
                if !quiet {
                    self.show(&msg, true);
                }
                None
            }
        };
        self.refresh();
        res
    }

    /// Abre archivos con enlaces (`.txt`, `.html`…) y los manda al
    /// capturador.
    fn analyze_files(&self, files: &[PathBuf]) {
        let mut text = String::new();
        for f in files {
            match read_link_file(f) {
                Ok(urls) => text.extend(urls.iter().map(|u| format!("{u}\n"))),
                Err(msg) => {
                    self.show(&msg, true);
                    return;
                }
            }
        }
        self.analyze(&text, false);
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
            if self.analyze_in(text, true, false, batch).is_some() {
                self.pop.borrow_mut().more_links();
            }
            return;
        }
        if let Some(batch) = self.analyze_in(text, true, true, 0) {
            self.pop.borrow_mut().open(batch, mode == 1, Instant::now());
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
                let (status, usable, tone) = popup::status(i);
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
        let res = self.grabber.take(Pick::Batch(batch), self.dest(), start);
        self.close_popup();
        self.taken(res, start, false);
    }

    /// Pasa lo marcado del capturador a la cola.
    fn grab_to_queue(&self, start: bool) {
        let res = self.grabber.take(Pick::Selected, self.dest(), start);
        self.taken(res, start, true);
    }

    /// Cuenta lo que pasó al llevar archivos a la cola.
    fn taken(&self, res: Result<TakeResponse, String>, start: bool, from_grabber: bool) {
        match res {
            Ok(r) if r.ids.is_empty() && r.errors.is_empty() => {
                if from_grabber {
                    self.show("No hay archivos marcados.", true);
                }
                return;
            }
            Ok(r) if r.errors.is_empty() => self.show("", false),
            Ok(r) => self.show(&r.errors.join(" "), true),
            Err(e) => {
                self.show(&e, true);
                return;
            }
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
        s.speed_limit = ui.get_speed_limit();
        s.max_per_server = ui.get_max_per_server();
        s.watch_clipboard = ui.get_watch_clipboard();
        s.keep_downloading = ui.get_keep_downloading();
        s.close_to_tray = ui.get_close_to_tray();
        s.minimize_to_tray = ui.get_minimize_to_tray();
        s.notify_done = ui.get_notify_done();
        s.clip_popup = ui.get_clip_popup();
        s.clip_popup_autoclose = ui.get_clip_autoclose();
        s.clip_popup_autostart = self.popup.get_autostart();
        s.download_dir = PathBuf::from(ui.get_download_dir().as_str());
        s.package_folders = ui.get_package_folders();
        s.single_file_folders = ui.get_single_file_folders();
        s.save();
        apply_queue_settings(&self.manager, &s);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();

    // Si ya hay una ventana abierta (quizá escondida en la bandeja), se le
    // pasan los argumentos y esta termina.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (instance_tx, instance_rx) = mpsc::channel();
    let _instance = match settings::instance_path() {
        Some(path) => match single::claim(&path, &args, instance_tx) {
            single::Claim::Forwarded => return Ok(()),
            single::Claim::Primary(i) => Some(i),
        },
        None => None,
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("omni-motor")
        .build()?;
    let settings = Settings::load();
    let manager = Arc::new(manager::open(rt.handle().clone()));
    if manager.in_process() {
        tracing::info!("el servicio corre dentro de la ventana");
    }
    let grabber = RemoteGrabber::new(rt.handle().clone(), manager.client().clone());

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

    let (tray_tx, tray_rx) = mpsc::channel();
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
        rt: rt.handle().clone(),
        tray: RefCell::new(None),
        tray_tx,
        tray_rx,
        instance_rx,
        hidden: Cell::new(false),
        notes: RefCell::default(),
    });

    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(250), {
        let app = app.clone();
        move || {
            app.handle_events();
            app.refresh();
        }
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

    app.open_args(args);
    app.refresh();

    // El icono de la bandeja, en cuanto arranque el bucle de eventos.
    Timer::single_shot(Duration::ZERO, {
        let app = app.clone();
        move || app.create_tray()
    });

    // El programa sigue con la ventana escondida en la bandeja: el bucle
    // termina con «Salir» o al cerrar la ventana si no hay bandeja.
    ui.show()?;
    slint::run_event_loop_until_quit()?;
    let _ = ui.hide();
    let _ = app.popup.hide();

    // Al salir: pausar todo (salvo "Seguir descargando al salir") y dar
    // tiempo al motor a guardar el progreso.
    drop(timer);
    drop(clip_timer);
    drop(app.tray.borrow_mut().take());
    app.save_settings();
    app.grabber.close();
    let keep = app.settings.borrow().keep_downloading;
    rt.block_on(manager.close(keep));
    rt.shutdown_timeout(Duration::from_secs(1));
    Ok(())
}

/// Lee los enlaces de un archivo `.txt`, `.html`…
fn read_link_file(path: &std::path::Path) -> Result<Vec<url::Url>, String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let len = std::fs::metadata(path)
        .map_err(|e| format!("No se pudo abrir {name}: {e}."))?
        .len();
    if len > omni_grab::MAX_LINK_FILE {
        return Err(format!(
            "{name} es demasiado grande para ser una lista de enlaces."
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("No se pudo leer {name}: {e}."))?;
    omni_grab::links_from_file(&name, &bytes)
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
            }
            app.show_window();
        }
    });
    p.on_discard({
        let app = app.clone();
        move || {
            let batch = app.pop.borrow().batch;
            app.grabber.command(GrabCommand::DiscardBatch { batch });
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
            app.grabber.command(GrabCommand::SelectItem {
                id: key as u64,
                selected: on,
            });
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
    // Cerrar la ventana principal la esconde en la bandeja o, si no hay
    // bandeja (o no se quiere), sale del programa.
    ui.window().on_close_requested({
        let app = app.clone();
        move || {
            app.close_requested();
            slint::CloseRequestResponse::HideWindow
        }
    });
}

/// Pasa a la cola lo que se elige en la ventana.
fn apply_queue_settings(manager: &Manager, s: &Settings) {
    manager.update_settings(|q| {
        q.max_active = s.max_active.max(1) as usize;
        q.connections = s.connections.max(1) as usize;
        // Un límite puesto desde la terminal (por ejemplo 300K) no está en
        // el desplegable: se respeta mientras no se elija otra opción.
        if settings::speed_limit_index(q.speed_limit) != s.speed_limit {
            q.speed_limit = settings::speed_limit_bytes(s.speed_limit);
        }
        q.max_per_server = s.max_per_server.max(0) as usize;
    });
}

/// La cola es del servicio, y la CLI también puede cambiarla: la ventana
/// enseña siempre lo que tiene el servicio.
fn sync_queue_settings(ui: &AppWindow, manager: &Manager) {
    if !manager.online() {
        return;
    }
    let q = manager.settings();
    let speed = settings::speed_limit_index(q.speed_limit);
    if ui.get_speed_limit() != speed {
        ui.set_speed_limit(speed);
    }
    let max_active = q.max_active.min(i32::MAX as usize) as i32;
    if ui.get_max_active() != max_active {
        ui.set_max_active(max_active);
    }
    let connections = q.connections.min(i32::MAX as usize) as i32;
    if ui.get_connections() != connections {
        ui.set_connections(connections);
    }
    let per_server = q.max_per_server.min(i32::MAX as usize) as i32;
    if ui.get_max_per_server() != per_server {
        ui.set_max_per_server(per_server);
    }
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
    ui.set_speed_limit(s.speed_limit);
    ui.set_max_per_server(s.max_per_server);
    ui.set_watch_clipboard(s.watch_clipboard);
    ui.set_keep_downloading(s.keep_downloading);
    ui.set_close_to_tray(s.close_to_tray);
    ui.set_minimize_to_tray(s.minimize_to_tray);
    ui.set_notify_done(s.notify_done);
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
    ui.on_add_links_file({
        let app = app.clone();
        move || {
            let Some(files) = rfd::FileDialog::new()
                .set_title("Abrir archivo con enlaces")
                .add_filter("Listas de enlaces", omni_grab::LINK_FILE_EXTENSIONS)
                .add_filter("Todos los archivos", &["*"])
                .pick_files()
            else {
                return;
            };
            if let Some(ui) = app.ui() {
                ui.set_add_dialog_open(false);
            }
            app.analyze_files(&files);
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
            let (id, selected) = (key as u64, on);
            app.grabber.command(if is_package {
                GrabCommand::SelectPackage { id, selected }
            } else {
                GrabCommand::SelectItem { id, selected }
            });
            app.refresh();
        }
    });
    ui.on_grab_select_all({
        let app = app.clone();
        move |on| {
            app.grabber.command(GrabCommand::SelectAll { selected: on });
            app.refresh();
        }
    });
    ui.on_grab_remove_offline({
        let app = app.clone();
        move || {
            app.grabber.command(GrabCommand::RemoveOffline);
            app.refresh();
        }
    });
    ui.on_grab_recheck({
        let app = app.clone();
        move || {
            app.grabber
                .command(GrabCommand::Recheck { ids: Vec::new() });
            app.refresh();
        }
    });
    ui.on_grab_clear({
        let app = app.clone();
        move || {
            app.grabber.command(GrabCommand::Clear);
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
                app.grabber.command(GrabCommand::RenamePackage {
                    id,
                    name: name.to_string(),
                });
            }
            app.refresh();
        }
    });
    ui.on_pkg_folder({
        let app = app.clone();
        move |folder| {
            if let Some(id) = app.state.borrow().grab_package() {
                app.grabber.command(GrabCommand::SetPackageFolder {
                    id,
                    folder: PathBuf::from(folder.trim()),
                });
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
                app.grabber
                    .command(GrabCommand::SetPackageFolder { id, folder: dir });
                app.refresh();
            }
        }
    });
    ui.on_change_pkg_priority({
        let app = app.clone();
        move |i| {
            if let Some(id) = app.state.borrow().grab_package() {
                app.grabber.command(GrabCommand::SetPackagePriority {
                    id,
                    priority: Priority::from_index(i as usize),
                });
            }
            app.refresh();
        }
    });
    ui.on_pkg_remove({
        let app = app.clone();
        move || {
            let id = app.state.borrow().grab_package();
            if let Some(id) = id {
                app.grabber.command(GrabCommand::RemovePackage { id });
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
