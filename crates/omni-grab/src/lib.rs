//! Capturador de enlaces: averigua qué archivos hay detrás de un enlace antes
//! de descargar nada, como el "LinkGrabber" de JDownloader.
//!
//! - Si el enlace es un archivo, devuelve su nombre, tamaño y si se puede
//!   reanudar ([`omni_core::RemoteInfo`]).
//! - Si es una página HTML, devuelve los enlaces de la página que parecen
//!   archivos (por su extensión), para comprobar cada uno con [`inspect`].
//!
//! No sigue páginas dentro de páginas: un nivel basta para listados de
//! carpetas y páginas de descarga sencillas. Los servicios que esconden el
//! archivo detrás de JavaScript o de una cuenta necesitarán los plugins de
//! hosts (fase 5).

use reqwest::header::CONTENT_TYPE;
use url::Url;

pub use omni_core::RemoteInfo;

/// Cuánto HTML leemos como mucho de una página.
const MAX_PAGE_BYTES: usize = 4 * 1024 * 1024;
/// Enlaces como mucho por página.
pub const MAX_LINKS_PER_PAGE: usize = 500;

/// Qué hay detrás de un enlace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Found {
    /// Un archivo descargable.
    File(RemoteInfo),
    /// Una página web, con los enlaces a archivos que contiene.
    Page { url: Url, links: Vec<Url> },
}

/// Mira qué hay detrás de `url`: un archivo o una página con enlaces.
pub async fn inspect(client: &reqwest::Client, url: &Url) -> omni_core::Result<Found> {
    // Primero un GET normal para ver el tipo de contenido. Si es un archivo,
    // soltamos la respuesta sin leerla y lo sondeamos como hace el motor
    // (con rango), que es lo que dice de verdad si se puede reanudar.
    let resp = client.get(url.clone()).send().await?;
    let is_html = resp.status().is_success()
        && resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(is_html_type);
    if !is_html {
        drop(resp);
        return omni_core::probe(client, url).await.map(Found::File);
    }

    let page_url = resp.url().clone();
    let html = read_limited(resp, MAX_PAGE_BYTES).await?;
    let mut links = extract_links(&html, &page_url);
    links.truncate(MAX_LINKS_PER_PAGE);
    Ok(Found::Page {
        url: page_url,
        links,
    })
}

fn is_html_type(ct: &str) -> bool {
    let ct = ct.split(';').next().unwrap_or("").trim();
    ct.eq_ignore_ascii_case("text/html") || ct.eq_ignore_ascii_case("application/xhtml+xml")
}

async fn read_limited(mut resp: reqwest::Response, max: usize) -> omni_core::Result<String> {
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        let room = max - buf.len();
        buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if buf.len() >= max {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Saca las URLs http(s) de un texto pegado: una o varias, separadas por
/// espacios, saltos de línea o metidas en una frase.
pub fn extract_urls(text: &str) -> Vec<Url> {
    let mut out: Vec<Url> = Vec::new();
    let mut rest = text;
    while let Some(start) = find_scheme(rest) {
        let tail = &rest[start..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`'))
            .unwrap_or(tail.len());
        // Signos de puntuación pegados al final de una frase no son parte de
        // la URL.
        let candidate = tail[..end].trim_end_matches(['.', ',', ';', ':', ')', ']', '!', '?']);
        if let Ok(u) = Url::parse(candidate)
            && u.host_str().is_some()
            && !out.contains(&u)
        {
            out.push(u);
        }
        rest = &tail[end.max(1)..];
    }
    out
}

fn find_scheme(s: &str) -> Option<usize> {
    let lower = s.to_ascii_lowercase();
    match (lower.find("http://"), lower.find("https://")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Extensiones de páginas web y de sus recursos (estilos, scripts, fuentes
/// web), no de archivos que alguien quiera descargar.
const PAGE_EXTENSIONS: &[&str] = &[
    "html", "htm", "xhtml", "php", "asp", "aspx", "jsp", "cgi", "pl", "shtml", "css", "js", "mjs",
    "map", "ico", "woff", "woff2",
];

/// Saca de una página los enlaces (`href`) que parecen archivos: con
/// extensión y que no sea de página web. Resuelve los relativos con `base`,
/// quita el `#fragmento` y no repite.
pub fn extract_links(html: &str, base: &Url) -> Vec<Url> {
    let mut out: Vec<Url> = Vec::new();
    for raw in href_values(html) {
        let value = decode_entities(raw.trim());
        if value.is_empty() || value.starts_with('#') {
            continue;
        }
        let Ok(mut url) = base.join(&value) else {
            continue;
        };
        if !matches!(url.scheme(), "http" | "https") {
            continue;
        }
        url.set_fragment(None);
        if looks_like_file(&url) && !out.contains(&url) {
            out.push(url);
        }
    }
    out
}

fn looks_like_file(url: &Url) -> bool {
    let Some(last) = url.path_segments().and_then(|mut s| s.next_back()) else {
        return false;
    };
    let Some((stem, ext)) = last.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && !ext.is_empty()
        && ext.len() <= 8
        && ext.chars().all(|c| c.is_ascii_alphanumeric())
        && !PAGE_EXTENSIONS.iter().any(|p| ext.eq_ignore_ascii_case(p))
}

/// Valores de los atributos `href`, con o sin comillas.
fn href_values(html: &str) -> Vec<&str> {
    let lower = html.to_ascii_lowercase();
    let bytes = html.as_bytes();
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(found) = lower[pos..].find("href") {
        let mut i = pos + found + 4;
        pos = i;
        // El carácter anterior no puede ser parte de otro nombre
        // ("data-href").
        let before = (pos - 4).checked_sub(1).map(|b| bytes[b]);
        if before.is_some_and(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            continue;
        }
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            continue;
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let (start, end) = match bytes[i] {
            q @ (b'"' | b'\'') => {
                let start = i + 1;
                match html[start..].find(q as char) {
                    Some(len) => (start, start + len),
                    None => break,
                }
            }
            _ => {
                let start = i;
                let len = html[start..]
                    .find(|c: char| c.is_whitespace() || c == '>')
                    .unwrap_or(html.len() - start);
                (start, start + len)
            }
        };
        out.push(&html[start..end]);
        pos = end;
    }
    out
}

/// Las entidades HTML que aparecen en URLs.
fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&#38;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(v: Vec<Url>) -> Vec<String> {
        v.into_iter().map(String::from).collect()
    }

    #[test]
    fn urls_from_pasted_text() {
        let text = "Mira esto: https://a.com/x.zip, y también\nhttp://b.org/y.iso.\n\
                    https://a.com/x.zip repetido y (https://c.net/z.7z) ftp://no.com/a";
        assert_eq!(
            urls(extract_urls(text)),
            [
                "https://a.com/x.zip",
                "http://b.org/y.iso",
                "https://c.net/z.7z"
            ]
        );
        assert!(extract_urls("nada por aquí").is_empty());
        assert_eq!(urls(extract_urls("HTTPS://A.COM/B")), ["https://a.com/B"]);
    }

    #[test]
    fn links_from_page() {
        let base = Url::parse("https://ejemplo.com/descargas/index.html").unwrap();
        let html = r##"
            <a href="programa-1.2.exe">Windows</a>
            <a HREF='/otros/linux.tar.gz'>Linux</a>
            <a href=mac.dmg>Mac</a>
            <a href="https://cdn.ejemplo.com/big.iso?token=1&amp;t=2#x">ISO</a>
            <a href="programa-1.2.exe">repetido</a>
            <a href="otra-pagina.html">página</a>
            <a href="carpeta/">carpeta</a>
            <a href="#arriba">ancla</a>
            <a href="mailto:yo@ejemplo.com">correo</a>
            <a data-href="trampa.zip">no</a>
            <link href="estilo.css">
        "##;
        assert_eq!(
            urls(extract_links(html, &base)),
            [
                "https://ejemplo.com/descargas/programa-1.2.exe",
                "https://ejemplo.com/otros/linux.tar.gz",
                "https://ejemplo.com/descargas/mac.dmg",
                "https://cdn.ejemplo.com/big.iso?token=1&t=2",
            ]
        );
    }
}
