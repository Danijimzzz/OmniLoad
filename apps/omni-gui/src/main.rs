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

mod automation;
mod clipboard;
mod format;
mod manager;
mod notify;
mod platform;
mod popup;
mod settings;
mod single;
mod smooth;
mod tray;
mod update;
mod views;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use slint::winit_030::WinitWindowAccessor;
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

use automation::{FinishAction, FinishWatch};
use manager::{Manager, Priority};
use omni_api::{RemoteGrabber, TakeResponse};
use omni_grab::{Check, Dest, GrabCommand, Pick};
use settings::{Settings, Table};
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
    /// Dirección de la versión nueva, si hay.
    release_url: RefCell<Option<String>>,
    /// Icono de la bandeja (si el sistema tiene bandeja).
    tray: RefCell<Option<Tray>>,
    tray_tx: Sender<TrayEvent>,
    tray_rx: Receiver<TrayEvent>,
    /// Argumentos de otras ventanas que se abrieron estando esta en marcha.
    instance_rx: Receiver<Vec<String>>,
    /// La ventana está escondida en la bandeja.
    hidden: Cell<bool>,
    notes: RefCell<notify::Tracker>,
    finish: RefCell<FinishWatch>,
    /// Lo último que se pintó: si la cola no cambió no se repinta (con miles
    /// de filas, rehacer la tabla cuatro veces por segundo es mucho trabajo).
    last_rows: RefCell<Vec<manager::RowInfo>>,
    /// Cuenta atrás de suspender o apagar: cuándo y qué.
    countdown: Cell<Option<(Instant, FinishAction)>>,
    /// Si el inicio con el sistema está puesto (se lee una vez al arrancar).
    autostart_on: Cell<bool>,
    rule_rows: Rc<VecModel<RuleRow>>,
    col_rows: Rc<VecModel<ColState>>,
    grab_col_rows: Rc<VecModel<ColState>>,
    /// Arrancó con `--minimizado` (inicio con el sistema).
    start_hidden: bool,
}

/// Segundos que se dan para cancelar suspender o apagar.
const COUNTDOWN: Duration = Duration::from_secs(60);

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
            rules: s
                .file_rules
                .iter()
                .filter_map(|r| omni_grab::FileRule::parse(&r.extensions, r.folder.clone()))
                .collect(),
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
        self.repaint(&ui, rows, true);
    }

    /// Cada cuarto de segundo: la tabla de descargas solo se rehace si la cola
    /// cambió.
    fn tick(&self) {
        let rows = self.manager.snapshot();
        let changed = *self.last_rows.borrow() != rows || self.hidden.get();
        if let Some(ui) = self.ui() {
            self.repaint(&ui, rows, changed);
        }
    }

    fn repaint(&self, ui: &AppWindow, rows: Vec<manager::RowInfo>, changed: bool) {
        if changed {
            self.notify_finished(ui, &rows);
            self.watch_finish(&rows);
            self.update_tray(ui, &rows);
        }
        if self.hidden.get() {
            // Escondida en la bandeja: no hace falta pintar las tablas (con
            // miles de filas se notaría), pero el aviso de enlaces sigue.
            self.refresh_popup();
            *self.last_rows.borrow_mut() = rows;
            return;
        }
        let grabbed = self.grabber.snapshot();
        let dest = self.dest();
        let mut st = self.state.borrow_mut();
        if changed {
            views::fill_downloads(ui, &self.dl_rows, &rows, &mut st);
        }
        views::fill_grabber(ui, &self.grab_rows, &grabbed, &dest, &mut st);
        drop(st);
        *self.last_rows.borrow_mut() = rows;
        sync_queue_settings(ui, &self.manager);
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

    fn save_columns(&self) {
        let mut s = self.settings.borrow_mut();
        s.columns = read_columns(&self.col_rows);
        s.grab_columns = read_columns(&self.grab_col_rows);
        s.save();
    }

    /// Al terminar todas las descargas, hace lo que se eligió en Ajustes.
    fn watch_finish(&self, rows: &[manager::RowInfo]) {
        if !self.finish.borrow_mut().update(rows) {
            return;
        }
        let action = FinishAction::from_index(self.settings.borrow().finish_action);
        match action {
            FinishAction::Nothing => {}
            FinishAction::Quit => {
                self.close_popup();
                let _ = slint::quit_event_loop();
            }
            FinishAction::Suspend | FinishAction::Shutdown => {
                self.countdown
                    .set(Some((Instant::now() + COUNTDOWN, action)));
                notify::send(
                    &self.rt,
                    notify::Note {
                        title: "Descargas terminadas".into(),
                        body: format!(
                            "Se va a {} en 60 segundos. Abre OmniLoad para cancelar.",
                            action.describe()
                        ),
                    },
                );
            }
        }
    }

    /// Lleva la cuenta atrás y la enseña en la ventana.
    fn tick_countdown(&self) {
        let Some((until, action)) = self.countdown.get() else {
            return;
        };
        let left = until.saturating_duration_since(Instant::now()).as_secs();
        if Instant::now() >= until {
            self.countdown.set(None);
            if let Some(ui) = self.ui() {
                ui.set_finish_text("".into());
            }
            automation::run_action(action);
        } else if let Some(ui) = self.ui() {
            ui.set_finish_text(
                format!(
                    "Descargas terminadas: se va a {} en {} s.",
                    action.describe(),
                    left + 1
                )
                .into(),
            );
        }
    }

    fn cancel_countdown(&self) {
        self.countdown.set(None);
        if let Some(ui) = self.ui() {
            ui.set_finish_text("".into());
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

    /// Mira en GitHub si hay una versión nueva y lo cuenta en Ajustes › Versión
    /// (y en la barra de arriba si la hay). `quiet`: al abrir, sin
    /// «Comprobando…» ni mensajes si falla.
    fn check_updates(self: &Rc<Self>, quiet: bool) {
        let Some(ui) = self.ui() else { return };
        if ui.get_update_state() == 1 {
            return;
        }
        ui.set_update_state(1);
        ui.set_update_text("Comprobando si hay una versión nueva…".into());
        let (tx, rx) = mpsc::channel();
        self.rt.spawn(async move {
            let _ = tx.send(update::check().await);
        });
        // El resultado llega por el canal; se recoge con un temporizador
        // corto que se para solo.
        let app = self.clone();
        let timer = Rc::new(Timer::default());
        let keep = timer.clone();
        timer.start(TimerMode::Repeated, Duration::from_millis(200), move || {
            let Ok(outcome) = rx.try_recv() else { return };
            keep.stop();
            app.show_update(&outcome, quiet);
        });
    }

    fn show_update(&self, outcome: &update::Outcome, quiet: bool) {
        use update::Outcome::*;
        let Some(ui) = self.ui() else { return };
        let (state, text) = match outcome {
            Newer { version, url } => {
                *self.release_url.borrow_mut() = Some(url.clone());
                (
                    3,
                    format!(
                        "Hay una versión nueva: {version}. Tienes la {}. Actualizar es opcional.",
                        update::CURRENT
                    ),
                )
            }
            UpToDate { .. } => (2, "Tienes la última versión.".to_owned()),
            NoReleases => (
                4,
                "Aún no hay versiones publicadas en GitHub: esta es la más reciente.".to_owned(),
            ),
            Failed(e) if quiet => {
                tracing::info!("no se pudo buscar una versión nueva: {e}");
                (0, String::new())
            }
            Failed(e) => (5, format!("No se pudo comprobar: {e}.")),
        };
        ui.set_update_state(state);
        ui.set_update_text(text.into());
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
        self.tick_countdown();
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
        if self.start_hidden {
            if self.tray.borrow().is_some() {
                self.hidden.set(true);
            } else {
                // Sin bandeja no habría forma de llegar a la ventana.
                self.show_window();
            }
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
            // Se desvanece poco a poco y, al terminar, se cierra.
            self.pop.borrow_mut().start_fade(now);
            if self.pop.borrow().fade_finished(now) {
                self.close_popup();
                return;
            }
        } else if self.pop.borrow().is_fading() {
            // El ratón volvió a pasar por encima, o llegaron más enlaces.
            self.pop.borrow_mut().cancel_fade();
        }
        self.popup.set_fading(self.pop.borrow().is_fading());
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
        s.font_level = theme.get_font_level();
        s.blur_level = theme.get_blur_level();
        s.ui_scale = ui.get_ui_scale();
        s.side_width = ui.get_side_w();
        s.nav_width = ui.get_nav_w();
        s.reduce_transparency = theme.get_reduce_transparency();
        s.properties_panel = ui.get_properties_panel();
        s.max_active = ui.get_max_active();
        s.connections = ui.get_connections();
        s.max_per_server = ui.get_max_per_server();
        s.watch_clipboard = ui.get_watch_clipboard();
        s.keep_downloading = ui.get_keep_downloading();
        s.check_updates = ui.get_check_updates();
        s.close_to_tray = ui.get_close_to_tray();
        s.minimize_to_tray = ui.get_minimize_to_tray();
        s.notify_done = ui.get_notify_done();
        s.clip_popup = ui.get_clip_popup();
        s.clip_popup_autoclose = ui.get_clip_autoclose();
        s.clip_popup_autostart = self.popup.get_autostart();
        s.download_dir = PathBuf::from(ui.get_download_dir().as_str());
        s.package_folders = ui.get_package_folders();
        s.single_file_folders = ui.get_single_file_folders();
        s.schedule_enabled = ui.get_schedule_enabled();
        s.schedule_days = ui.get_schedule_days();
        s.schedule_from = ui.get_schedule_from().to_string();
        s.schedule_to = ui.get_schedule_to().to_string();
        s.finish_action = ui.get_finish_action();
        ui.set_schedule_error(s.schedule().is_none());
        // Solo se toca el sistema cuando la casilla cambió (consultar el
        // registro en cada ajuste sería lento e innecesario).
        let autostart = ui.get_autostart();
        if autostart != self.autostart_on.get() {
            match automation::set_autostart(autostart) {
                Ok(()) => self.autostart_on.set(autostart),
                Err(e) => {
                    tracing::warn!("no se pudo cambiar el inicio con el sistema: {e}");
                    ui.set_autostart(self.autostart_on.get());
                }
            }
        }
        s.save();
        apply_queue_settings(&self.manager, &s);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // La escala de la interfaz elegida en Ajustes (si no se fuerza por entorno).
    // Slint la lee al crear la ventana.
    if std::env::var_os("SLINT_SCALE_FACTOR").is_none()
        && let Some(f) = settings::ui_scale_factor(Settings::load().ui_scale)
    {
        // SAFETY: es lo primero que hace el programa, todavía con un solo hilo.
        unsafe { std::env::set_var("SLINT_SCALE_FACTOR", f.to_string()) };
    }
    init_logging();

    // Si ya hay una ventana abierta (quizá escondida en la bandeja), se le
    // pasan los argumentos y esta termina.
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // Al iniciar con el sistema arranca escondida en la bandeja.
    let start_hidden = args.iter().any(|a| a == automation::AUTOSTART_FLAG);
    args.retain(|a| a != automation::AUTOSTART_FLAG);
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
    let mut settings = Settings::load();
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

    let rule_rows: Rc<VecModel<RuleRow>> = Rc::new(VecModel::default());
    fill_rules(&rule_rows, &settings.file_rules);
    ui.set_rules(ModelRc::from(rule_rows.clone()));

    let col_rows: Rc<VecModel<ColState>> = Rc::new(VecModel::default());
    settings.columns = settings::fix_columns(Table::Downloads, &settings.columns);
    fill_columns(Table::Downloads, &col_rows, &settings.columns);
    ui.global::<DlCols>()
        .set_cols(ModelRc::from(col_rows.clone()));
    let grab_col_rows: Rc<VecModel<ColState>> = Rc::new(VecModel::default());
    settings.grab_columns = settings::fix_columns(Table::Grabber, &settings.grab_columns);
    fill_columns(Table::Grabber, &grab_col_rows, &settings.grab_columns);
    ui.global::<GrabCols>()
        .set_cols(ModelRc::from(grab_col_rows.clone()));

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
        release_url: RefCell::new(None),
        tray: RefCell::new(None),
        tray_tx,
        tray_rx,
        instance_rx,
        hidden: Cell::new(false),
        notes: RefCell::default(),
        finish: RefCell::default(),
        last_rows: RefCell::default(),
        countdown: Cell::new(None),
        autostart_on: Cell::new(automation::autostart_enabled()),
        rule_rows,
        col_rows,
        grab_col_rows,
        start_hidden,
    });

    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(250), {
        let app = app.clone();
        move || {
            app.handle_events();
            app.tick();
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
    connect_updates(&ui, &app);
    ui.set_app_version(update::base_version(update::CURRENT).into());
    ui.set_app_pr(update::pr_number(update::CURRENT).map_or(0, |n| n as i32));
    if app.settings.borrow().check_updates {
        app.check_updates(true);
    }

    app.open_args(args);
    app.refresh();

    // El icono de la bandeja, en cuanto arranque el bucle de eventos.
    Timer::single_shot(Duration::ZERO, {
        let app = app.clone();
        move || app.create_tray()
    });

    // El programa sigue con la ventana escondida en la bandeja: el bucle
    // termina con «Salir» o al cerrar la ventana si no hay bandeja.
    if !start_hidden {
        ui.show()?;
    }
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
    to.set_font_level(from.get_font_level());
    to.set_blur_level(from.get_blur_level());
}

fn connect_updates(ui: &AppWindow, app: &Rc<App>) {
    ui.on_check_updates_now({
        let app = app.clone();
        move || app.check_updates(false)
    });
    ui.on_open_release({
        let app = app.clone();
        move || {
            let url = app
                .release_url
                .borrow()
                .clone()
                .unwrap_or_else(update::releases_page);
            if let Err(e) = platform::open_url(&url) {
                app.show(&format!("No se pudo abrir {url}: {e}"), true);
            }
        }
    });
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
        q.max_per_server = s.max_per_server.max(0) as usize;
        // Con una hora mal escrita se deja el horario como estaba.
        if let Some(schedule) = s.schedule() {
            q.schedule = schedule;
        }
    });
}

/// La cola es del servicio, y la CLI también puede cambiarla: la ventana
/// enseña siempre lo que tiene el servicio.
fn sync_queue_settings(ui: &AppWindow, manager: &Manager) {
    if !manager.online() {
        return;
    }
    let q = manager.settings();
    // El límite también lo cambia la terminal; no se pisa lo que se está
    // escribiendo.
    if !ui.get_speed_editing() && settings::parse_speed(&ui.get_speed_text()) != Ok(q.speed_limit) {
        ui.set_speed_text(settings::speed_text(q.speed_limit).into());
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
    theme.set_font_level(s.font_level);
    theme.set_blur_level(s.blur_level);
    ui.set_ui_scale(s.ui_scale);
    ui.set_side_w(s.side_width.clamp(240.0, 720.0));
    ui.set_nav_w(s.nav_width.clamp(180.0, 420.0));
    theme.set_reduce_transparency(s.reduce_transparency);
    ui.set_properties_panel(s.properties_panel);
    ui.set_max_active(s.max_active);
    ui.set_connections(s.connections);
    ui.set_max_per_server(s.max_per_server);
    ui.set_watch_clipboard(s.watch_clipboard);
    ui.set_keep_downloading(s.keep_downloading);
    ui.set_check_updates(s.check_updates);
    ui.set_close_to_tray(s.close_to_tray);
    ui.set_minimize_to_tray(s.minimize_to_tray);
    ui.set_notify_done(s.notify_done);
    ui.set_clip_popup(s.clip_popup);
    ui.set_clip_autoclose(s.clip_popup_autoclose);
    ui.set_download_dir(s.download_dir.display().to_string().into());
    ui.set_package_folders(s.package_folders);
    ui.set_single_file_folders(s.single_file_folders);
    ui.set_schedule_enabled(s.schedule_enabled);
    set_schedule_days(ui, s.schedule_days);
    ui.set_schedule_from(s.schedule_from.clone().into());
    ui.set_schedule_to(s.schedule_to.clone().into());
    ui.set_finish_action(s.finish_action);
    ui.set_autostart(automation::autostart_enabled());
}

/// Los días del horario: el número y las siete casillas que se ven.
fn set_schedule_days(ui: &AppWindow, mask: i32) {
    ui.set_schedule_days(mask);
    let on: Vec<bool> = (0..7).map(|d| mask & (1 << d) != 0).collect();
    ui.set_schedule_day_on(ModelRc::from(Rc::new(VecModel::from(on))));
}

/// Pone las columnas guardadas en el modelo de la tabla.
fn fill_columns(table: Table, model: &VecModel<ColState>, cols: &[settings::ColCfg]) {
    model.set_vec(
        cols.iter()
            .map(|c| ColState {
                id: c.id,
                name: settings::column_name(table, c.id).into(),
                width: c.width,
                shown: c.shown,
            })
            .collect::<Vec<_>>(),
    );
}

/// Lo que hay ahora en el modelo, para guardarlo.
fn read_columns(model: &VecModel<ColState>) -> Vec<settings::ColCfg> {
    model
        .iter()
        .map(|c| settings::ColCfg {
            id: c.id,
            width: c.width,
            shown: c.shown,
        })
        .collect()
}

/// Pone al día la lista de reglas de Ajustes › Automático.
fn fill_rules(model: &VecModel<RuleRow>, rules: &[settings::RuleCfg]) {
    let rows: Vec<RuleRow> = rules
        .iter()
        .map(|r| RuleRow {
            exts: r.extensions.as_str().into(),
            folder: r.folder.display().to_string().into(),
        })
        .collect();
    model.set_vec(rows);
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
        move |delete_files| {
            let ids = app.selected_ids();
            if delete_files {
                let keep = app.settings.borrow().download_dir.clone();
                app.manager.remove_with_files(&ids, keep);
            } else {
                app.manager.remove(&ids);
            }
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
    // Columnas: arrastrar el borde, mostrar/ocultar y cambiar el orden.
    macro_rules! table_cols {
        ($global:ident, $rows:ident) => {
            ui.global::<$global>().on_resize({
                let app = app.clone();
                move |i, width| {
                    if let Some(mut c) = app.$rows.row_data(i as usize) {
                        c.width = width.max(settings::MIN_COL_WIDTH);
                        app.$rows.set_row_data(i as usize, c);
                    }
                }
            });
            ui.global::<$global>().on_resize_done({
                let app = app.clone();
                move || app.save_columns()
            });
            ui.global::<$global>().on_reorder({
                let app = app.clone();
                move |from, x| {
                    let from = from as usize;
                    let mut all: Vec<ColState> = app.$rows.iter().collect();
                    if from >= all.len() {
                        return;
                    }
                    // Destino: la columna visible sobre la que se suelta.
                    let mut edge = 0.0;
                    let mut to = None;
                    for (i, c) in all.iter().enumerate() {
                        if c.shown {
                            edge += c.width;
                            if x < edge {
                                to = Some(i);
                                break;
                            }
                        }
                    }
                    let to = to.unwrap_or_else(|| all.iter().rposition(|c| c.shown).unwrap_or(0));
                    if to == from {
                        return;
                    }
                    let c = all.remove(from);
                    all.insert(to, c);
                    app.$rows.set_vec(all);
                    app.save_columns();
                }
            });
        };
    }
    table_cols!(DlCols, col_rows);
    table_cols!(GrabCols, grab_col_rows);
    ui.on_col_toggle({
        let app = app.clone();
        move |i| {
            let i = i as usize;
            if let Some(mut c) = app.col_rows.row_data(i) {
                c.shown = !c.shown;
                app.col_rows.set_row_data(i, c);
                app.save_columns();
            }
        }
    });
    ui.on_col_move({
        let app = app.clone();
        move |i, dir| {
            let (i, j) = (i as usize, i + dir);
            if j < 0 || j as usize >= app.col_rows.row_count() {
                return;
            }
            let j = j as usize;
            if let (Some(a), Some(b)) = (app.col_rows.row_data(i), app.col_rows.row_data(j)) {
                app.col_rows.set_row_data(i, b);
                app.col_rows.set_row_data(j, a);
                app.save_columns();
            }
        }
    });
    ui.on_col_reset({
        let app = app.clone();
        move || {
            fill_columns(
                Table::Downloads,
                &app.col_rows,
                &settings::default_columns(Table::Downloads),
            );
            app.save_columns();
        }
    });
    // Barra de título propia: mover, redimensionar, minimizar, maximizar, cerrar.
    ui.on_win_drag({
        let app = app.clone();
        move || {
            if let Some(ui) = app.ui() {
                ui.window().with_winit_window(|w| {
                    let _ = w.drag_window();
                });
            }
        }
    });
    ui.on_win_resize({
        let app = app.clone();
        move |edge| {
            use slint::winit_030::winit::window::ResizeDirection as D;
            let dir = match edge {
                0 => D::North,
                1 => D::NorthEast,
                2 => D::East,
                3 => D::SouthEast,
                4 => D::South,
                5 => D::SouthWest,
                6 => D::West,
                _ => D::NorthWest,
            };
            if let Some(ui) = app.ui() {
                ui.window().with_winit_window(|w| {
                    let _ = w.drag_resize_window(dir);
                });
            }
        }
    });
    ui.on_win_minimize({
        let app = app.clone();
        move || {
            if let Some(ui) = app.ui() {
                // Solo el botón de la propia barra esconde en la bandeja. Una
                // minimización de fuera (Win+D, la barra de tareas) deja la
                // ventana minimizada de verdad, para que el sistema pueda
                // restaurarla.
                if app.settings.borrow().minimize_to_tray && app.tray_available() {
                    app.hide_to_tray();
                } else {
                    ui.window().set_minimized(true);
                }
            }
        }
    });
    ui.on_win_toggle_maximize({
        let app = app.clone();
        move || {
            if let Some(ui) = app.ui() {
                let w = ui.window();
                w.set_maximized(!w.is_maximized());
                ui.set_maximized(w.is_maximized());
            }
        }
    });
    ui.on_win_close({
        let app = app.clone();
        move || app.close_requested()
    });
    ui.on_speed_text_accepted({
        let app = app.clone();
        move |text| {
            let Some(ui) = app.ui() else { return };
            match settings::parse_speed(&text) {
                Ok(limit) => {
                    app.manager.update_settings(|q| q.speed_limit = limit);
                    ui.set_speed_text(settings::speed_text(limit).into());
                    app.show("", false);
                }
                Err(()) => {
                    app.show(
                        "No entiendo ese límite. Escribe, por ejemplo, 700 KB/s, 3 MB/s o «Sin límite».",
                        true,
                    );
                    let current = app.manager.settings().speed_limit;
                    ui.set_speed_text(settings::speed_text(current).into());
                }
            }
        }
    });
    ui.on_speed_preset({
        let app = app.clone();
        move |i| {
            let Some(ui) = app.ui() else { return };
            let limit = settings::speed_limit_bytes(i);
            app.manager.update_settings(|q| q.speed_limit = limit);
            ui.set_speed_text(settings::speed_text(limit).into());
        }
    });
    ui.on_schedule_day_toggled({
        let app = app.clone();
        move |day| {
            let Some(ui) = app.ui() else { return };
            set_schedule_days(&ui, ui.get_schedule_days() ^ (1 << day));
            app.save_settings();
        }
    });
    ui.on_rule_add({
        let app = app.clone();
        move || {
            app.settings
                .borrow_mut()
                .file_rules
                .push(settings::RuleCfg::default());
            fill_rules(&app.rule_rows, &app.settings.borrow().file_rules);
        }
    });
    ui.on_rule_remove({
        let app = app.clone();
        move |i| {
            let mut s = app.settings.borrow_mut();
            if (i as usize) < s.file_rules.len() {
                s.file_rules.remove(i as usize);
            }
            s.save();
            fill_rules(&app.rule_rows, &s.file_rules);
        }
    });
    ui.on_rule_exts_edited({
        let app = app.clone();
        move |i, text| {
            // La fila de la lista no se rehace: así no se pierde el cursor.
            let mut s = app.settings.borrow_mut();
            if let Some(r) = s.file_rules.get_mut(i as usize) {
                r.extensions = text.to_string();
            }
            s.save();
        }
    });
    ui.on_rule_choose({
        let app = app.clone();
        move |i| {
            let current = app
                .settings
                .borrow()
                .file_rules
                .get(i as usize)
                .map(|r| r.folder.display().to_string())
                .unwrap_or_default();
            if let Some(dir) = pick_folder(&current) {
                let mut s = app.settings.borrow_mut();
                if let Some(r) = s.file_rules.get_mut(i as usize) {
                    r.folder = dir;
                }
                s.save();
                fill_rules(&app.rule_rows, &s.file_rules);
            }
        }
    });
    ui.on_finish_cancel({
        let app = app.clone();
        move || app.cancel_countdown()
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
