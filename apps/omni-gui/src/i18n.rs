//! Idioma de la interfaz: español (el de los textos originales) o inglés.
//!
//! Los textos de la ventana van marcados con `@tr` en `ui/app.slint` y el
//! inglés está en `lang/en/LC_MESSAGES/omni-gui.po`. Los que se escriben desde
//! Rust (estados, avisos, menú de la bandeja…) pasan por [`tr`], que cambia
//! las frases conocidas por su versión en inglés.

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};

static ENGLISH: AtomicBool = AtomicBool::new(false);

/// ¿La interfaz está en inglés?
pub fn is_english() -> bool {
    ENGLISH.load(Ordering::Relaxed)
}

/// Qué idioma toca para la opción de Ajustes: 0 automático (inglés salvo que
/// el sistema esté en español), 1 español, 2 inglés.
pub fn wants_english(option: i32) -> bool {
    match option {
        1 => false,
        2 => true,
        _ => !sys_locale::get_locale().is_some_and(|l| l.to_lowercase().starts_with("es")),
    }
}

/// Pone el idioma elegido (también en los textos de Slint).
pub fn apply(option: i32) {
    let english = wants_english(option);
    ENGLISH.store(english, Ordering::Relaxed);
    if let Err(e) = slint::select_bundled_translation(if english { "en" } else { "" }) {
        tracing::warn!("no se pudo cambiar el idioma de la ventana: {e}");
    }
}

/// Frases escritas desde Rust, de la más larga a la más corta (para que una
/// frase entera gane a sus trozos).
const PHRASES: &[(&str, &str)] = &[
    ("Hay una versión nueva: ", "A new version is available: "),
    (". Tienes la ", ". You have "),
    (". Actualizar es opcional.", ". Updating is optional."),
    (
        "Aún no hay versiones publicadas en GitHub: esta es la más reciente.",
        "No versions are published on GitHub yet: this is the latest.",
    ),
    (
        "Comprobando si hay una versión nueva…",
        "Checking for a new version…",
    ),
    (
        "Descargada y comprobada. OmniLoad se cierra para instalarla y se abrirá solo.",
        "Downloaded and verified. OmniLoad closes to install it and will reopen by itself.",
    ),
    ("Descargando la actualización…", "Downloading the update…"),
    ("Tienes la última versión.", "You have the latest version."),
    ("No se pudo comprobar: ", "Could not check: "),
    ("No se pudo actualizar: ", "Could not update: "),
    ("No se pudo descargar", "Download failed"),
    ("No se pudo abrir ", "Could not open "),
    ("No se pudo leer ", "Could not read "),
    (
        "es demasiado grande para ser una lista de enlaces.",
        "is too big to be a list of links.",
    ),
    ("No hay archivos marcados.", "No files are selected."),
    (
        "No entiendo ese límite. Escribe, por ejemplo, 700 KB/s, 3 MB/s o «Sin límite».",
        "I don't understand that limit. Write, for example, 700 KB/s, 3 MB/s or “No limit”.",
    ),
    (
        "OmniLoad sigue en la bandeja",
        "OmniLoad is still in the tray",
    ),
    (
        "Las descargas continúan. Para salir del todo, usa «Salir» en el menú del icono.",
        "Downloads continue. To quit completely, use “Quit” in the icon menu.",
    ),
    (
        "Descargas terminadas: se va a ",
        "Downloads finished: going to ",
    ),
    ("Se va a ", "Going to "),
    (
        " en 60 segundos. Abre OmniLoad para cancelar.",
        " in 60 seconds. Open OmniLoad to cancel.",
    ),
    ("cerrar OmniLoad", "close OmniLoad"),
    ("suspender el equipo", "suspend the computer"),
    ("apagar el equipo", "shut down the computer"),
    ("Descargas terminadas", "Downloads finished"),
    ("Descarga terminada", "Download finished"),
    ("Paquete terminado", "Package finished"),
    ("Descargas en pausa", "Downloads paused"),
    ("Sin descargas en curso", "No downloads in progress"),
    ("Analizando 1 enlace…", "Analyzing 1 link…"),
    (" enlaces…", " links…"),
    ("Analizando ", "Analyzing "),
    ("Comprobando… ", "Checking… "),
    ("Comprobado en ", "Checked in "),
    ("Comprobando…", "Checking…"),
    ("Ya en Descargas", "Already in Downloads"),
    ("Ya está en Descargas", "Already in Downloads"),
    ("En línea · una conexión", "Online · one connection"),
    ("En línea", "Online"),
    ("No disponible: ", "Unavailable: "),
    ("Disponible", "Available"),
    ("Caído", "Offline"),
    ("Red del sistema", "System network"),
    ("sin actividad", "idle"),
    (" descargando", " downloading"),
    (" en espera", " waiting"),
    (" terminadas", " finished"),
    (" con error", " with errors"),
    (" archivos", " files"),
    ("Descargando · ", "Downloading · "),
    (" conex.", " conn."),
    ("(máx. ", "(max. "),
    ("Conectando…", "Connecting…"),
    ("Pausando…", "Pausing…"),
    ("Pausada", "Paused"),
    ("Pausado", "Paused"),
    ("Sin iniciar", "Not started"),
    ("Completada", "Completed"),
    (" completados)", " completed)"),
    ("Enlace caducado", "Link expired"),
    ("En pausa", "On hold"),
    ("En espera", "Waiting"),
    ("Descargando", "Downloading"),
    (" errores", " errors"),
    ("Tiempo estimado", "Estimated time"),
    ("Tamaño total", "Total size"),
    ("Descargado", "Downloaded"),
    ("Restante", "Remaining"),
    ("Paquetes", "Packages"),
    ("Archivos", "Files"),
    ("Paquete", "Package"),
    ("Carpeta", "Folder"),
    ("Enlace", "Link"),
    ("Estado", "Status"),
    ("Tamaño", "Size"),
    ("Conexiones", "Connections"),
    ("Detalle", "Detail"),
    ("Prioridad", "Priority"),
    ("Abrir OmniLoad", "Open OmniLoad"),
    ("Sin límite", "No limit"),
    ("Nombre", "Name"),
    ("Progreso", "Progress"),
    ("Velocidad", "Speed"),
    ("Disponibilidad", "Availability"),
    ("Guardar en", "Save to"),
    ("Iniciar descargas", "Start downloads"),
    ("Pausar descargas", "Pause downloads"),
    ("Vigilar portapapeles", "Watch clipboard"),
    ("Salir", "Quit"),
    ("Descargas", "Downloads"),
    ("Error", "Error"),
];

/// El texto en el idioma elegido: en español no cambia nada; en inglés se
/// sustituyen las frases conocidas.
pub fn tr(text: &str) -> Cow<'_, str> {
    if !is_english() {
        return Cow::Borrowed(text);
    }
    Cow::Owned(to_english(text))
}

fn to_english(text: &str) -> String {
    let mut out = text.to_owned();
    for (es, en) in PHRASES {
        if out.contains(es) {
            out = out.replace(es, en);
        }
    }
    out
}

/// Para mensajes que llevan rutas o nombres: solo cambia las frases enteras
/// (nunca palabras sueltas, que podrían estar dentro de un nombre).
pub fn tr_msg(text: &str) -> Cow<'_, str> {
    if !is_english() {
        return Cow::Borrowed(text);
    }
    let mut out = text.to_owned();
    for (es, en) in PHRASES {
        if es.len() >= 14 && es.contains(' ') && out.contains(es) {
            out = out.replace(es, en);
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_phrases() {
        assert_eq!(
            to_english("Descargando · 3 conex. (máx. 2)"),
            "Downloading · 3 conn. (max. 2)"
        );
        assert_eq!(to_english("Paquete"), "Package");
        assert_eq!(to_english("Pausada · 1 error"), "Paused · 1 error");
        // Sin inglés activado no se toca nada.
        assert_eq!(tr("Paquete"), "Paquete");
    }
}
