//! Convierte el estado del gestor y del capturador en filas y textos de la
//! ventana: árbol de paquetes, resumen, gráfica de velocidad y panel de
//! propiedades.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use slint::{ModelRc, SharedString, VecModel};

use crate::manager::{RowInfo, State, sizes};
use crate::{AppWindow, DlRow, GrabTreeRow, Stat, format, sync_model};
use omni_grab::{Check, Dest, GrabItem, GrabPackage};

/// Colores de estado (los traduce el tema de Slint).
const TONE_ACCENT: i32 = 0;
const TONE_OK: i32 = 1;
const TONE_WARN: i32 = 2;
const TONE_ERR: i32 = 3;
const TONE_MUTED: i32 = 4;

/// Muestras de la gráfica de velocidad (una por segundo).
const HISTORY: usize = 60;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DlSelection {
    Package(String),
    File(u64),
}

/// Lo que recuerda la ventana entre refrescos: paquetes plegados, selección,
/// búsquedas e historial de velocidad.
#[derive(Default)]
pub struct UiState {
    pub dl_selected: Option<DlSelection>,
    /// Carpeta de lo seleccionado, para "Abrir carpeta".
    pub dl_selected_folder: Option<PathBuf>,
    pub dl_search: String,
    dl_collapsed: HashSet<String>,
    /// Paquetes de la tabla de descargas en el orden pintado; la clave de una
    /// fila de paquete es su posición aquí.
    dl_packages: Vec<String>,
    grab_selected: Option<u64>,
    pub grab_search: String,
    grab_collapsed: HashSet<u64>,
    /// Lo último que se escribió en los campos del panel del paquete, para no
    /// pisar lo que el usuario está tecleando.
    pushed_pkg: Option<(u64, String, String)>,
    speed: VecDeque<f64>,
    last_sample: Option<Instant>,
}

impl UiState {
    pub fn select_download(&mut self, key: i32, is_package: bool) {
        self.dl_selected = if is_package {
            self.dl_packages
                .get(key as usize)
                .map(|n| DlSelection::Package(n.clone()))
        } else {
            Some(DlSelection::File(key as u64))
        };
    }

    pub fn toggle_download_package(&mut self, key: i32) {
        if let Some(name) = self.dl_packages.get(key as usize)
            && !self.dl_collapsed.remove(name)
        {
            self.dl_collapsed.insert(name.clone());
        }
    }

    pub fn select_grab(&mut self, key: i32, _is_package: bool) {
        // Al pinchar un archivo, se elige su paquete (las propiedades son del
        // paquete); `fill_grabber` traduce el id del archivo.
        self.grab_selected = Some(key as u64);
    }

    pub fn toggle_grab_package(&mut self, key: i32) {
        let id = key as u64;
        if !self.grab_collapsed.remove(&id) {
            self.grab_collapsed.insert(id);
        }
    }

    /// El paquete del capturador elegido.
    pub fn grab_package(&self) -> Option<u64> {
        self.grab_selected
    }
}

fn matches(search: &str, text: &str) -> bool {
    search.is_empty() || text.to_lowercase().contains(&search.to_lowercase())
}

fn file_tone(r: &RowInfo) -> i32 {
    match r.state {
        State::Starting | State::Running => TONE_ACCENT,
        State::Done => TONE_OK,
        State::Pausing => TONE_WARN,
        State::Paused if r.started_once => TONE_WARN,
        State::Failed => TONE_ERR,
        State::Queued | State::Paused => TONE_MUTED,
    }
}

fn pct(f: Option<f32>) -> (f32, SharedString) {
    match f {
        Some(f) => (f, format!("{:.0} %", f * 100.0).into()),
        None => (-1.0, SharedString::new()),
    }
}

fn speed_text(rate: f64) -> SharedString {
    if rate > 0.0 {
        format::speed(rate).into()
    } else {
        SharedString::new()
    }
}

fn eta_text(eta: Option<Duration>) -> SharedString {
    eta.map(|d| format::duration(d).into()).unwrap_or_default()
}

/// Estado de un paquete a partir de sus archivos.
fn package_status(files: &[&RowInfo]) -> (String, i32) {
    let n = files.len();
    let errors = files.iter().filter(|r| r.state == State::Failed).count();
    let done = files.iter().filter(|r| r.state == State::Done).count();
    let (mut text, tone) = if errors == n {
        ("Error".to_owned(), TONE_ERR)
    } else if files.iter().any(|r| r.state.is_active()) {
        ("Descargando".to_owned(), TONE_ACCENT)
    } else if done == n {
        ("Completada".to_owned(), TONE_OK)
    } else if files.iter().any(|r| r.state == State::Queued) {
        let held = files.iter().any(|r| r.held);
        (
            if held { "En pausa" } else { "En espera" }.to_owned(),
            TONE_MUTED,
        )
    } else if files.iter().any(|r| r.started_once) {
        ("Pausado".to_owned(), TONE_WARN)
    } else {
        ("Sin iniciar".to_owned(), TONE_MUTED)
    };
    if errors > 0 && errors < n {
        text.push_str(&format!(
            " · {errors} {}",
            if errors == 1 { "error" } else { "errores" }
        ));
        return (text, if tone == TONE_ACCENT { tone } else { TONE_WARN });
    }
    (text, tone)
}

fn hosts_text<'a>(hosts: impl Iterator<Item = &'a str>) -> String {
    let set: HashSet<&str> = hosts.filter(|h| !h.is_empty()).collect();
    match set.len() {
        0 => String::new(),
        1 => set.into_iter().next().unwrap_or_default().to_owned(),
        n => format!("{n} hosts"),
    }
}

fn stat(label: &str, value: impl Into<SharedString>) -> Stat {
    Stat {
        label: label.into(),
        value: value.into(),
    }
}

/// Tabla de descargas, resumen, cabecera y panel de propiedades.
pub fn fill_downloads(ui: &AppWindow, model: &VecModel<DlRow>, rows: &[RowInfo], st: &mut UiState) {
    // Paquetes en el orden en que llegaron.
    let mut packages: Vec<(String, Vec<&RowInfo>)> = Vec::new();
    for r in rows {
        match packages.iter_mut().find(|(n, _)| *n == r.package) {
            Some((_, v)) => v.push(r),
            None => packages.push((r.package.clone(), vec![r])),
        }
    }
    st.dl_packages = packages.iter().map(|(n, _)| n.clone()).collect();
    st.dl_collapsed.retain(|n| st.dl_packages.contains(n));

    let mut out = Vec::new();
    for (key, (name, files)) in packages.iter().enumerate() {
        let pkg_match = matches(&st.dl_search, name);
        let shown: Vec<&&RowInfo> = files
            .iter()
            .filter(|r| pkg_match || matches(&st.dl_search, &r.name))
            .collect();
        if shown.is_empty() {
            continue;
        }
        let total: u64 = files.iter().filter_map(|r| r.total).sum();
        let downloaded: u64 = files
            .iter()
            .map(|r| r.downloaded.min(r.total.unwrap_or(u64::MAX)))
            .sum();
        let rate: f64 = files.iter().map(|r| r.rate).sum();
        let fraction = (total > 0).then(|| (downloaded as f64 / total as f64).min(1.0) as f32);
        let (fraction, progress) = pct(fraction);
        let eta = (rate > 0.0 && total > downloaded)
            .then(|| Duration::from_secs_f64((total - downloaded) as f64 / rate));
        let (status, tone) = package_status(files);
        let expanded = !st.dl_collapsed.contains(name) || !st.dl_search.is_empty();
        out.push(DlRow {
            key: key as i32,
            is_package: true,
            expanded,
            selected: st.dl_selected == Some(DlSelection::Package(name.clone())),
            name: format!("{name} ({})", files.len()).into(),
            host: hosts_text(files.iter().map(|r| r.host.as_str())).into(),
            size: if total > 0 {
                format::bytes(total).into()
            } else {
                SharedString::new()
            },
            fraction,
            progress,
            speed: speed_text(rate),
            eta: eta_text(eta),
            priority: files
                .iter()
                .map(|r| r.priority)
                .min()
                .unwrap_or_default()
                .label()
                .into(),
            status: status.into(),
            tone,
        });
        if !expanded {
            continue;
        }
        for r in shown {
            let (fraction, progress) = pct(r.fraction());
            out.push(DlRow {
                key: r.id as i32,
                is_package: false,
                expanded: false,
                selected: st.dl_selected == Some(DlSelection::File(r.id)),
                name: r.name.clone().into(),
                host: r.host.clone().into(),
                size: sizes(r).into(),
                fraction,
                progress,
                speed: speed_text(r.rate),
                eta: eta_text(r.eta()),
                priority: r.priority.label().into(),
                status: r.status().into(),
                tone: file_tone(r),
            });
        }
    }
    sync_model(model, out);

    // Resumen.
    let total: u64 = rows.iter().filter_map(|r| r.total).sum();
    let downloaded: u64 = rows
        .iter()
        .map(|r| r.downloaded.min(r.total.unwrap_or(u64::MAX)))
        .sum();
    let rate: f64 = rows.iter().map(|r| r.rate).sum();
    let remaining = total.saturating_sub(downloaded);
    let eta = if rate > 0.0 && remaining > 0 {
        format::duration(Duration::from_secs_f64(remaining as f64 / rate))
    } else {
        "—".into()
    };
    let totals = vec![
        stat("Paquetes", packages.len().to_string()),
        stat("Archivos", rows.len().to_string()),
        stat("Tamaño total", format::bytes(total)),
        stat("Descargado", format::bytes(downloaded)),
        stat("Restante", format::bytes(remaining)),
        stat("Tiempo estimado", eta),
    ];
    ui.set_totals(ModelRc::new(VecModel::from(totals)));
    ui.set_net_text(
        if rate > 0.0 {
            format!("Red del sistema · {}", format::speed(rate))
        } else {
            "Red del sistema · sin actividad".to_owned()
        }
        .into(),
    );
    ui.set_header_speed(if rate > 0.0 {
        format::speed(rate).into()
    } else {
        SharedString::new()
    });

    // Gráfica: una muestra por segundo.
    let now = Instant::now();
    if st
        .last_sample
        .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1))
    {
        st.last_sample = Some(now);
        st.speed.push_back(rate);
        while st.speed.len() > HISTORY {
            st.speed.pop_front();
        }
    }
    ui.set_speed_path(sparkline(&st.speed).into());

    fill_selection(ui, rows, st);
}

/// Comandos de `Path` para la gráfica de 140×30.
fn sparkline(samples: &VecDeque<f64>) -> String {
    let max = samples.iter().copied().fold(0.0_f64, f64::max);
    let n = samples.len();
    if n < 2 || max <= 0.0 {
        return "M 0 28 L 140 28".into();
    }
    let step = 140.0 / (HISTORY - 1) as f64;
    let x0 = 140.0 - step * (n - 1) as f64;
    let mut s = String::new();
    for (i, v) in samples.iter().enumerate() {
        let x = x0 + step * i as f64;
        let y = 28.0 - v / max * 24.0;
        s.push_str(&format!(
            "{} {x:.1} {y:.1} ",
            if i == 0 { "M" } else { "L" }
        ));
    }
    s
}

fn fill_selection(ui: &AppWindow, rows: &[RowInfo], st: &mut UiState) {
    let mut lines = Vec::new();
    let (kind, title, prio, can_start, can_stop, folder) = match &st.dl_selected {
        Some(DlSelection::File(id)) => match rows.iter().find(|r| r.id == *id) {
            Some(r) => {
                lines.push(stat("Paquete", r.package.clone()));
                lines.push(stat("Carpeta", r.folder.display().to_string()));
                lines.push(stat("Enlace", r.url.clone()));
                let mut state = r.status();
                if let Some(f) = r.fraction()
                    && r.state != State::Done
                {
                    state.push_str(&format!(" · {:.0} %", f * 100.0));
                }
                lines.push(stat("Estado", state));
                if r.state.is_active() {
                    let mut c = format!("{} de {} activas", r.receiving, r.connections);
                    if r.server_limit > 0 && r.server_limit < r.connections {
                        c.push_str(&format!(" · el servidor solo acepta {}", r.server_limit));
                    }
                    if r.retries > 0 {
                        c.push_str(&format!(" · {} reintentos", r.retries));
                    }
                    lines.push(stat("Conexiones", c));
                }
                if !r.detail.is_empty() {
                    lines.push(stat("Detalle", r.detail.clone()));
                }
                (
                    2,
                    r.name.clone(),
                    r.priority.index(),
                    matches!(r.state, State::Paused | State::Failed),
                    r.state == State::Queued || r.state.is_active(),
                    Some(r.folder.clone()),
                )
            }
            None => (0, String::new(), 1, false, false, None),
        },
        Some(DlSelection::Package(name)) => {
            let files: Vec<&RowInfo> = rows.iter().filter(|r| &r.package == name).collect();
            if files.is_empty() {
                (0, String::new(), 1, false, false, None)
            } else {
                let done = files.iter().filter(|r| r.state == State::Done).count();
                lines.push(stat(
                    "Archivos",
                    format!("{} ({done} completados)", files.len()),
                ));
                lines.push(stat("Carpeta", files[0].folder.display().to_string()));
                let total: u64 = files.iter().filter_map(|r| r.total).sum();
                let got: u64 = files.iter().map(|r| r.downloaded).sum();
                lines.push(stat(
                    "Tamaño",
                    format!(
                        "{} de {}",
                        format::bytes(got.min(total)),
                        format::bytes(total)
                    ),
                ));
                lines.push(stat("Estado", package_status(&files).0));
                (
                    1,
                    name.clone(),
                    files
                        .iter()
                        .map(|r| r.priority)
                        .min()
                        .unwrap_or_default()
                        .index(),
                    files
                        .iter()
                        .any(|r| matches!(r.state, State::Paused | State::Failed)),
                    files
                        .iter()
                        .any(|r| r.state == State::Queued || r.state.is_active()),
                    Some(files[0].folder.clone()),
                )
            }
        }
        None => (0, String::new(), 1, false, false, None),
    };
    if kind == 0 {
        st.dl_selected = None;
    }
    st.dl_selected_folder = folder;
    ui.set_sel_kind(kind);
    ui.set_sel_title(title.into());
    ui.set_sel_lines(ModelRc::new(VecModel::from(lines)));
    ui.set_sel_priority(prio as i32);
    ui.set_sel_can_start(can_start);
    ui.set_sel_can_stop(can_stop);
}

fn avail(item: &GrabItem) -> (String, i32) {
    if item.duplicate && matches!(item.check, Check::Online { .. }) {
        return ("Ya está en Descargas".into(), TONE_WARN);
    }
    match &item.check {
        Check::Checking => ("Comprobando…".into(), TONE_MUTED),
        Check::Online { resumable: true } => ("En línea".into(), TONE_OK),
        Check::Online { resumable: false } => ("En línea · una conexión".into(), TONE_OK),
        Check::Offline(e) => (format!("No disponible: {e}"), TONE_ERR),
    }
}

/// Tabla del capturador, resumen y panel del paquete.
pub fn fill_grabber(
    ui: &AppWindow,
    model: &VecModel<GrabTreeRow>,
    packages: &[GrabPackage],
    dest: &Dest,
    st: &mut UiState,
) {
    // La selección puede ser un archivo: se traduce a su paquete.
    if let Some(sel) = st.grab_selected
        && !packages.iter().any(|p| p.id == sel)
    {
        st.grab_selected = packages
            .iter()
            .find(|p| p.items.iter().any(|i| i.id == sel))
            .map(|p| p.id);
    }
    if st.grab_selected.is_none() {
        st.grab_selected = packages.first().map(|p| p.id);
    }
    st.grab_collapsed
        .retain(|id| packages.iter().any(|p| p.id == *id));

    let mut out = Vec::new();
    let (mut n_items, mut n_sel, mut sel_bytes) = (0, 0, 0u64);
    let mut n_offline = 0;
    for p in packages {
        n_items += p.items.len();
        n_offline += p
            .items
            .iter()
            .filter(|i| matches!(i.check, Check::Offline(_)))
            .count();
        for i in p.items.iter().filter(|i| i.selected) {
            n_sel += 1;
            sel_bytes += i.size.unwrap_or(0);
        }
        let pkg_match = matches(&st.grab_search, &p.name);
        let shown: Vec<_> = p
            .items
            .iter()
            .filter(|i| pkg_match || matches(&st.grab_search, &i.name))
            .collect();
        if shown.is_empty() {
            continue;
        }
        let online = p
            .items
            .iter()
            .filter(|i| matches!(i.check, Check::Online { .. }))
            .count();
        let checking = p
            .items
            .iter()
            .filter(|i| i.check == Check::Checking)
            .count();
        let mut avail_text = format!("{online} de {} en línea", p.items.len());
        if checking > 0 {
            avail_text.push_str(&format!(" · comprobando {checking}"));
        }
        let tone = if checking > 0 {
            TONE_MUTED
        } else if online == p.items.len() {
            TONE_OK
        } else {
            TONE_WARN
        };
        let size: u64 = p.items.iter().filter_map(|i| i.size).sum();
        let expanded = !st.grab_collapsed.contains(&p.id) || !st.grab_search.is_empty();
        let checked = p.items.iter().any(|i| i.selected);
        out.push(GrabTreeRow {
            key: p.id as i32,
            is_package: true,
            expanded,
            selected: st.grab_selected == Some(p.id),
            checked,
            dim: !checked,
            name: format!("{} ({})", p.name, p.items.len()).into(),
            host: hosts_text(p.items.iter().map(|i| i.host.as_str())).into(),
            size: if size > 0 {
                format::bytes(size).into()
            } else {
                SharedString::new()
            },
            avail: avail_text.into(),
            tone,
            priority: p.priority.label().into(),
            dest: p.folder(dest).display().to_string().into(),
        });
        if !expanded {
            continue;
        }
        for i in shown {
            let (text, tone) = avail(i);
            out.push(GrabTreeRow {
                key: i.id as i32,
                is_package: false,
                expanded: false,
                selected: false,
                checked: i.selected,
                dim: !i.selected,
                name: i.name.clone().into(),
                host: i.host.clone().into(),
                size: i.size.map(format::bytes).unwrap_or_default().into(),
                avail: text.into(),
                tone,
                priority: SharedString::new(),
                dest: SharedString::new(),
            });
        }
    }
    sync_model(model, out);

    ui.set_grab_count(n_items as i32);
    ui.set_grab_selected_count(n_sel);
    ui.set_grab_offline_count(n_offline as i32);
    ui.set_grab_summary(
        format!(
            "{n_sel} de {n_items} archivos{}",
            if sel_bytes > 0 {
                format!(" · {}", format::bytes(sel_bytes))
            } else {
                String::new()
            }
        )
        .into(),
    );

    // Panel del paquete.
    let sel = st
        .grab_selected
        .and_then(|id| packages.iter().find(|p| p.id == id));
    ui.set_pkg_selected(sel.is_some());
    if let Some(p) = sel {
        let folder = p.folder(dest).display().to_string();
        // Se reescribe un campo al cambiar de paquete, o si el valor cambió por
        // otro lado (por ejemplo, la carpeta automática al renombrar) y no es
        // ya lo que pone el campo. Así no se pisa lo que se está tecleando.
        let same_pkg = st.pushed_pkg.as_ref().is_some_and(|(id, ..)| *id == p.id);
        let (old_name, old_folder) = match &st.pushed_pkg {
            Some((_, n, f)) if same_pkg => (n.as_str(), f.as_str()),
            _ => ("", ""),
        };
        if !same_pkg || (old_name != p.name && ui.get_pkg_name_text().trim() != p.name.trim()) {
            ui.set_pkg_name_text(p.name.clone().into());
        }
        if !same_pkg || (old_folder != folder && ui.get_pkg_folder_text().trim() != folder) {
            ui.set_pkg_folder_text(folder.clone().into());
        }
        st.pushed_pkg = Some((p.id, p.name.clone(), folder));
        ui.set_pkg_priority(p.priority.index() as i32);
        ui.set_pkg_title(p.name.clone().into());
    } else {
        st.pushed_pkg = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparkline_scales_to_box() {
        let flat: VecDeque<f64> = [0.0, 0.0].into_iter().collect();
        assert_eq!(sparkline(&flat), "M 0 28 L 140 28");
        let s: VecDeque<f64> = [0.0, 10.0].into_iter().collect();
        let path = sparkline(&s);
        assert!(path.starts_with('M'), "{path}");
        assert!(path.trim_end().ends_with("140.0 4.0"), "{path}");
    }
}
