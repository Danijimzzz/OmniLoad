//! Vigilar el portapapeles: cuando se copia algo con enlaces, va al
//! capturador (como en JDownloader).
//!
//! Se consulta una vez por segundo desde el hilo de la ventana. Además del
//! texto se lee el HTML que deja el navegador al copiar un trozo de página:
//! así también llegan los enlaces escondidos detrás de palabras ("descarga
//! aquí"). Si el sistema no deja leer el portapapeles (por ejemplo, Linux sin
//! escritorio), no hace nada.

pub struct Watcher {
    clipboard: Option<arboard::Clipboard>,
    last: Option<String>,
    primed: bool,
}

impl Watcher {
    pub fn new() -> Self {
        Self {
            clipboard: arboard::Clipboard::new()
                .map_err(|e| tracing::info!("sin acceso al portapapeles: {e}"))
                .ok(),
            last: None,
            primed: false,
        }
    }

    /// Los enlaces copiados desde la última consulta, uno por línea. Lo que
    /// ya estaba copiado al abrir la ventana (o al activar la vigilancia) no
    /// cuenta.
    pub fn poll(&mut self, enabled: bool) -> Option<String> {
        if !enabled {
            self.primed = false;
            return None;
        }
        let clipboard = self.clipboard.as_mut()?;
        let text = clipboard.get_text().ok();
        if !self.primed {
            self.primed = true;
            self.last = text;
            return None;
        }
        if text.is_none() || text == self.last {
            return None;
        }
        self.last = text.clone();
        let html = clipboard.get().html().ok();
        let urls = omni_grab::clipboard_urls(text.as_deref().unwrap_or(""), html.as_deref());
        if urls.is_empty() {
            return None;
        }
        Some(
            urls.iter()
                .map(|u| u.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}
