//! Enlaces guardados en archivos: listas `.txt`, páginas `.html` guardadas y
//! contenedores de otros gestores.

use url::Url;

use crate::links::{extract_hrefs, extract_urls};

/// Tamaño máximo de un archivo de enlaces.
pub const MAX_LINK_FILE: u64 = 16 * 1024 * 1024;

/// Extensiones que se ofrecen al abrir un archivo de enlaces.
pub const LINK_FILE_EXTENSIONS: &[&str] = &["txt", "html", "htm", "crawljob", "dlc", "torrent"];

/// Un archivo `.torrent` del disco, como enlace `file:///…` (el plugin de
/// torrents lo lee al comprobarlo y al descargar). `None` si no es un
/// `.torrent`.
pub fn torrent_file_url(path: &std::path::Path) -> Option<Url> {
    let is_torrent = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("torrent"));
    let abs = std::path::absolute(path).ok()?;
    is_torrent.then(|| Url::from_file_path(abs).ok()).flatten()
}

/// Cambia las rutas de archivos `.torrent` que haya en el texto (una por
/// línea, con o sin comillas, como las copia el Explorador) por su enlace
/// `file:///…`. Solo las que existen; el resto del texto queda igual.
pub fn torrent_paths_to_urls(text: &str) -> String {
    text.lines()
        .map(|line| {
            let path = std::path::Path::new(line.trim().trim_matches(['"', '\'']));
            if path.is_file()
                && let Some(url) = torrent_file_url(path)
            {
                url.to_string()
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Saca los enlaces de un archivo, por su nombre y contenido.
///
/// - `.txt`, `.crawljob` y cualquier texto: las URLs que aparezcan.
/// - `.html`/`.htm`: los enlaces de la página y las URLs del texto.
/// - `.dlc`: contenedor cifrado de JDownloader. Para abrirlo hay que pedir
///   la clave a un servidor de JDownloader, cosa que OmniLoad no hace; se
///   explica al usuario en vez de fallar sin más.
pub fn links_from_file(name: &str, bytes: &[u8]) -> Result<Vec<Url>, String> {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    if ext == "dlc" {
        return Err("Los archivos .dlc van cifrados con una clave que solo da \
                    JDownloader, así que OmniLoad no puede abrirlos. Si la web \
                    ofrece los enlaces sueltos o en .txt, cópialos o abre ese archivo."
            .into());
    }
    let text = String::from_utf8_lossy(bytes);
    let looks_html = matches!(ext.as_str(), "html" | "htm" | "xhtml")
        || text
            .get(..text.len().min(1024))
            .is_some_and(|head| head.to_ascii_lowercase().contains("<html"));
    let mut urls = extract_urls(&text);
    if looks_html {
        for u in extract_hrefs(&text, None) {
            if !urls.contains(&u) {
                urls.push(u);
            }
        }
    }
    if urls.is_empty() {
        return Err(format!("En {name} no hay enlaces http o https."));
    }
    Ok(urls)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn torrent_paths_become_links() {
        let dir = std::env::temp_dir().join("omni-torrent-paths");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("mi peli.torrent");
        std::fs::write(&f, b"x").unwrap();
        let text = format!(
            "\"{}\"\nhttps://a.com/x.zip\n/no/existe.torrent",
            f.display()
        );
        let out = torrent_paths_to_urls(&text);
        let mut lines = out.lines();
        assert!(lines.next().unwrap().starts_with("file:///"));
        assert_eq!(lines.next(), Some("https://a.com/x.zip"));
        assert_eq!(lines.next(), Some("/no/existe.torrent"));
    }

    #[test]
    fn text_and_html_files() {
        let txt = b"https://a.com/1.zip\r\nhttps://a.com/2.zip\n# comentario\n";
        assert_eq!(links_from_file("lista.TXT", txt).unwrap().len(), 2);

        let html = br#"<html><body><a href="https://b.com/x.iso">x</a>
            texto https://c.com/y.7z</body></html>"#;
        let urls = links_from_file("guardada.html", html).unwrap();
        assert_eq!(urls.len(), 2);
        // Sin extensión, se reconoce el HTML por el contenido.
        assert_eq!(links_from_file("sin-extension", html).unwrap().len(), 2);

        let crawl = b"text=https://d.com/z.rar\npackageName=algo\n";
        assert_eq!(links_from_file("a.crawljob", crawl).unwrap().len(), 1);

        assert!(links_from_file("vacio.txt", b"nada").is_err());
        assert!(torrent_file_url(std::path::Path::new("/tmp/a.txt")).is_none());
        #[cfg(unix)]
        assert_eq!(
            torrent_file_url(std::path::Path::new("/tmp/mi pelicula.TORRENT"))
                .unwrap()
                .as_str(),
            "file:///tmp/mi%20pelicula.TORRENT"
        );
        assert!(
            links_from_file("cosas.dlc", b"abc")
                .unwrap_err()
                .contains("JDownloader")
        );
    }
}
