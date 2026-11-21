//! Vigilar el portapapeles: cuando se copia texto con enlaces, va al
//! capturador (como en JDownloader).
//!
//! Se consulta una vez por segundo desde el hilo de la ventana. Si el sistema
//! no deja leer el portapapeles (por ejemplo, Linux sin escritorio), no hace
//! nada.

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

    /// Texto nuevo con enlaces desde la última consulta. Lo que ya estaba
    /// copiado al abrir la ventana (o al activar la vigilancia) no cuenta.
    pub fn poll(&mut self, enabled: bool) -> Option<String> {
        if !enabled {
            self.primed = false;
            return None;
        }
        let text = self.clipboard.as_mut()?.get_text().ok();
        if !self.primed {
            self.primed = true;
            self.last = text;
            return None;
        }
        if text.is_none() || text == self.last {
            return None;
        }
        self.last = text.clone();
        text.filter(|t| t.contains("http://") || t.contains("https://"))
    }
}
