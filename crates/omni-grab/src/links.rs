//! Sacar enlaces de texto y de HTML.

use url::Url;

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

/// Todos los enlaces http(s) de un trozo de HTML, sean archivos o no: lo que
/// el usuario copió de una página (al copiar un texto con enlaces, el
/// portapapeles guarda también su HTML) o un archivo `.html` guardado. Los
/// relativos solo se pueden resolver si se conoce `base`.
pub fn extract_hrefs(html: &str, base: Option<&Url>) -> Vec<Url> {
    let mut out: Vec<Url> = Vec::new();
    for raw in href_values(html) {
        let value = decode_entities(raw.trim());
        let parsed = match base {
            Some(b) => b.join(&value),
            None => Url::parse(&value),
        };
        // El `#fragmento` se conserva: en Mega es la clave del archivo.
        let Ok(url) = parsed else { continue };
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            continue;
        }
        if !out.contains(&url) {
            out.push(url);
        }
    }
    out
}

/// Hosts de archivos conocidos: sus enlaces no tienen pinta de archivo
/// (`mega.nz/file/…`), pero lo son.
pub const FILE_HOSTS: &[&str] = &[
    "mega.nz",
    "mega.io",
    "gofile.io",
    "drive.google.com",
    "drive.usercontent.google.com",
    "docs.google.com",
    "mediafire.com",
    "1fichier.com",
    "pixeldrain.com",
    "dropbox.com",
    "we.tl",
    "wetransfer.com",
];

/// ¿Es un enlace de un host de archivos conocido?
pub fn is_file_host(url: &Url) -> bool {
    url.host_str().is_some_and(|h| {
        let h = h.trim_start_matches("www.");
        FILE_HOSTS
            .iter()
            .any(|f| h == *f || h.ends_with(&format!(".{f}")))
    })
}

/// Los enlaces de lo que hay en el portapapeles: los escritos en el texto y,
/// si el programa de origen dejó también HTML (un navegador, un correo), los
/// que van escondidos detrás de palabras. De estos solo los que parecen
/// archivos o son de hosts de archivos: al copiar un párrafo de una web para
/// pegarlo en otro sitio no se quiere que salte el capturador con los
/// enlaces del menú.
pub fn clipboard_urls(text: &str, html: Option<&str>) -> Vec<Url> {
    let mut out = extract_urls(text);
    let hidden = html.map(|h| extract_hrefs(h, None)).unwrap_or_default();
    for url in hidden {
        if (looks_like_file(&url) || is_file_host(&url)) && !out.contains(&url) {
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

/// El `<title>` de una página, limpio de espacios.
pub fn page_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let open = lower.find("<title")?;
    let start = open + lower[open..].find('>')? + 1;
    let end = start + lower[start..].find("</title")?;
    let title = decode_entities(&html[start..end]);
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    (!title.is_empty()).then_some(title)
}

/// Las entidades HTML que aparecen en URLs y títulos.
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
    fn title() {
        assert_eq!(
            page_title("<html><head><TITLE>\n Descargas &amp; más </title>").as_deref(),
            Some("Descargas & más")
        );
        assert_eq!(page_title("<title></title>"), None);
        assert_eq!(page_title("sin título"), None);
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

    #[test]
    fn hrefs_and_clipboard() {
        let html = r#"<p>Mira <a href="https://mega.nz/file/abc#clave">esto</a> y
            <a href='https://gofile.io/d/xyz'>esto</a>, <a href="/relativo.zip">no</a>
            <a href="javascript:void(0)">no</a> <a href="https://web.com/contacto">menú</a>
            <a href="https://www.mediafire.com/file/q1/x.zip/file">mf</a></p>"#;
        assert_eq!(
            urls(extract_hrefs(html, None)),
            [
                "https://mega.nz/file/abc#clave",
                "https://gofile.io/d/xyz",
                "https://web.com/contacto",
                "https://www.mediafire.com/file/q1/x.zip/file"
            ]
        );
        let base = Url::parse("https://web.com/a/").unwrap();
        assert_eq!(extract_hrefs(html, Some(&base)).len(), 5);
        assert_eq!(
            urls(clipboard_urls(
                "esto y esto, https://gofile.io/d/xyz",
                Some(html)
            )),
            [
                "https://gofile.io/d/xyz",
                "https://mega.nz/file/abc#clave",
                "https://www.mediafire.com/file/q1/x.zip/file"
            ]
        );
        assert!(clipboard_urls("nada", None).is_empty());
    }
}
