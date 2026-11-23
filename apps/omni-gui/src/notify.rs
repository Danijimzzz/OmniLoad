//! Notificaciones del sistema al terminar descargas.
//!
//! [`Tracker`] mira las fotos de la cola y dice qué avisar; no depende de
//! Slint ni del sistema, así se puede probar. [`send`] manda el aviso
//! (centro de notificaciones de Windows y macOS, D-Bus en Linux).

use std::collections::HashMap;

use crate::format;
use crate::manager::{RowInfo, State};

/// Un aviso para el usuario.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub title: String,
    pub body: String,
}

/// Más de estos avisos a la vez se juntan en uno.
const MAX_NOTES: usize = 3;

/// Recuerda el estado de cada descarga para avisar solo de los cambios.
#[derive(Debug, Default)]
pub struct Tracker {
    seen: HashMap<u64, State>,
    primed: bool,
}

impl Tracker {
    /// Compara con la foto anterior. La primera vez solo apunta: no avisa de
    /// lo que ya estaba terminado al abrir.
    pub fn update(&mut self, rows: &[RowInfo]) -> Vec<Note> {
        let mut done: Vec<&RowInfo> = Vec::new();
        let mut failed: Vec<&RowInfo> = Vec::new();
        if self.primed {
            for r in rows {
                let Some(&before) = self.seen.get(&r.id) else {
                    continue;
                };
                if before != State::Done && r.state == State::Done {
                    done.push(r);
                } else if before != State::Failed && r.state == State::Failed {
                    failed.push(r);
                }
            }
        }
        self.primed = true;
        self.seen = rows.iter().map(|r| (r.id, r.state)).collect();

        let mut notes = Vec::new();
        // Un aviso por paquete, cuando termina su último archivo.
        let mut packages: Vec<&str> = Vec::new();
        for r in &done {
            if !packages.contains(&r.package.as_str()) {
                packages.push(&r.package);
            }
        }
        for pkg in packages {
            let files: Vec<&RowInfo> = rows.iter().filter(|r| r.package == pkg).collect();
            if files.iter().any(|r| r.state != State::Done) {
                continue;
            }
            let size: u64 = files.iter().filter_map(|r| r.total).sum();
            notes.push(if let [one] = files.as_slice() {
                Note {
                    title: "Descarga terminada".into(),
                    body: with_size(&one.name, size),
                }
            } else {
                Note {
                    title: "Paquete terminado".into(),
                    body: with_size(&format!("{pkg} · {} archivos", files.len()), size),
                }
            });
        }
        for r in failed {
            notes.push(Note {
                title: "No se pudo descargar".into(),
                body: if r.detail.is_empty() {
                    r.name.clone()
                } else {
                    format!("{}: {}", r.name, r.detail)
                },
            });
        }
        if notes.len() > MAX_NOTES {
            let ok = notes.iter().filter(|n| !n.title.starts_with("No")).count();
            let bad = notes.len() - ok;
            let mut parts = Vec::new();
            if ok > 0 {
                parts.push(format!("{ok} terminadas"));
            }
            if bad > 0 {
                parts.push(format!("{bad} con error"));
            }
            notes = vec![Note {
                title: "Descargas".into(),
                body: parts.join(", "),
            }];
        }
        notes
    }
}

/// ¿Avisa de un fallo? (el aviso suelto de error o el resumen que cuenta
/// alguno).
pub fn is_failure(note: &Note) -> bool {
    note.title.starts_with("No se pudo") || note.body.contains("con error")
}

fn with_size(text: &str, size: u64) -> String {
    if size > 0 {
        format!("{text} ({})", format::bytes(size))
    } else {
        text.to_owned()
    }
}

/// El aviso en el idioma elegido. El título se traduce entero; el cuerpo solo
/// en las frases fijas, porque lleva nombres de archivo.
fn translated(note: &Note) -> (String, String) {
    if !crate::i18n::is_english() {
        return (note.title.clone(), note.body.clone());
    }
    let body = crate::i18n::tr_msg(&note.body)
        .replace(" archivos (", " files (")
        .replace(" terminadas, ", " finished, ")
        .replace(" con error", " with errors");
    (crate::i18n::tr(&note.title).into_owned(), body)
}

/// Manda el aviso al sistema, en otro hilo (en algunos sistemas tarda).
pub fn send(rt: &tokio::runtime::Handle, note: Note, sound: bool) {
    rt.spawn_blocking(move || {
        let mut n = notify_rust::Notification::new();
        let (title, body) = translated(&note);
        n.appname("OmniLoad").summary(&title).body(&body);
        if sound {
            // El nombre «default» lo entienden Windows y macOS; en Linux el
            // servidor de notificaciones decide.
            n.sound_name("default");
        }
        let result = n.show();
        if let Err(e) = result {
            tracing::warn!("no se pudo mostrar la notificación: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::Priority;

    fn row(id: u64, package: &str, state: State) -> RowInfo {
        RowInfo {
            id,
            package: package.into(),
            name: format!("archivo{id}.zip"),
            host: "a.com".into(),
            folder: "/tmp".into(),
            url: format!("https://a.com/{id}"),
            state,
            priority: Priority::Normal,
            held: false,
            downloaded: 0,
            total: Some(1024 * 1024),
            rate: 0.0,
            receiving: 0,
            connections: 0,
            server_limit: 0,
            retries: 0,
            started_once: true,
            speed_limit: None,
            detail: String::new(),
        }
    }

    #[test]
    fn nothing_on_first_look() {
        let mut t = Tracker::default();
        assert!(t.update(&[row(1, "a", State::Done)]).is_empty());
        assert!(t.update(&[row(1, "a", State::Done)]).is_empty());
    }

    #[test]
    fn single_file_package() {
        let mut t = Tracker::default();
        t.update(&[row(1, "a", State::Running)]);
        let notes = t.update(&[row(1, "a", State::Done)]);
        assert_eq!(
            notes,
            vec![Note {
                title: "Descarga terminada".into(),
                body: "archivo1.zip (1,0 MiB)".into()
            }]
        );
    }

    #[test]
    fn package_waits_for_last_file() {
        let mut t = Tracker::default();
        t.update(&[row(1, "p", State::Running), row(2, "p", State::Running)]);
        assert!(
            t.update(&[row(1, "p", State::Done), row(2, "p", State::Running)])
                .is_empty()
        );
        let notes = t.update(&[row(1, "p", State::Done), row(2, "p", State::Done)]);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].title, "Paquete terminado");
        assert_eq!(notes[0].body, "p · 2 archivos (2,0 MiB)");
    }

    #[test]
    fn failures_and_merging() {
        let mut t = Tracker::default();
        let before: Vec<_> = (1..=5)
            .map(|i| row(i, &i.to_string(), State::Running))
            .collect();
        t.update(&before);
        let mut after: Vec<_> = (1..=4)
            .map(|i| row(i, &i.to_string(), State::Done))
            .collect();
        let mut bad = row(5, "5", State::Failed);
        bad.detail = "404".into();
        after.push(bad);
        let notes = t.update(&after);
        assert_eq!(
            notes,
            vec![Note {
                title: "Descargas".into(),
                body: "4 terminadas, 1 con error".into()
            }]
        );

        let mut t = Tracker::default();
        t.update(&[row(9, "x", State::Running)]);
        let mut bad = row(9, "x", State::Failed);
        bad.detail = "404".into();
        assert_eq!(t.update(&[bad])[0].body, "archivo9.zip: 404");
    }

    #[test]
    fn new_rows_do_not_notify() {
        let mut t = Tracker::default();
        t.update(&[]);
        // Añadida ya terminada (por ejemplo, desde otra ventana): sin aviso.
        assert!(t.update(&[row(1, "a", State::Done)]).is_empty());
    }
}
