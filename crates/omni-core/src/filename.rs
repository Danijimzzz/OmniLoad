//! Nombre del archivo de destino: de `Content-Disposition` o de la URL, y
//! saneado para que sea válido en Windows, Linux y macOS a la vez.

use percent_encoding::percent_decode_str;
use url::Url;

const FALLBACK: &str = "descarga";
const MAX_LEN: usize = 200;

/// Extrae el nombre de una cabecera `Content-Disposition`. Prefiere
/// `filename*` (RFC 5987, con codificación) sobre `filename`.
pub fn from_content_disposition(header: &str) -> Option<String> {
    let mut plain = None;
    for part in header.split(';').map(str::trim) {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if key == "filename*" {
            // charset'idioma'valor-codificado
            let mut it = value.splitn(3, '\'');
            let (charset, _lang, encoded) = (it.next()?, it.next()?, it.next()?);
            let bytes: Vec<u8> = percent_decode_str(encoded).collect();
            let decoded = if charset.eq_ignore_ascii_case("utf-8") {
                String::from_utf8(bytes).ok()
            } else {
                // ISO-8859-1: cada byte es un carácter.
                Some(bytes.iter().map(|&b| b as char).collect())
            };
            if let Some(name) = decoded.filter(|n| !n.is_empty()) {
                return Some(name);
            }
        } else if key == "filename" {
            let v = value.trim_matches('"').to_string();
            if !v.is_empty() {
                plain = Some(v);
            }
        }
    }
    plain
}

/// Último segmento de la ruta de la URL, decodificado.
pub fn from_url(url: &Url) -> Option<String> {
    let last = url.path_segments()?.rev().find(|s| !s.is_empty())?;
    let decoded = percent_decode_str(last).decode_utf8_lossy().into_owned();
    (!decoded.is_empty()).then_some(decoded)
}

/// Deja un nombre válido en los tres sistemas: sin separadores de ruta ni
/// caracteres prohibidos en Windows, sin nombres reservados (`CON`, `NUL`…),
/// sin punto o espacio al final y con longitud razonable.
pub fn sanitize(name: &str) -> String {
    // Quedarse solo con el último componente por si viene una ruta.
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let mut out: String = base
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    while out.ends_with('.') || out.ends_with(' ') {
        out.pop();
    }
    let out = out.trim_start().to_string();
    if out.is_empty() || out == "." || out == ".." {
        return FALLBACK.to_string();
    }
    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    let mut out = if reserved { format!("_{out}") } else { out };
    if out.len() > MAX_LEN {
        // Recortar conservando la extensión y sin partir un carácter UTF-8.
        let ext = out
            .rfind('.')
            .map(|i| out[i..].to_string())
            .filter(|e| e.len() < 20)
            .unwrap_or_default();
        let mut cut = MAX_LEN - ext.len();
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out = format!("{}{}", &out[..cut], ext);
    }
    out
}

/// Elige el nombre final: `Content-Disposition`, luego la URL, luego
/// "descarga".
pub fn choose(content_disposition: Option<&str>, url: &Url) -> String {
    let raw = content_disposition
        .and_then(from_content_disposition)
        .or_else(|| from_url(url))
        .unwrap_or_else(|| FALLBACK.to_string());
    sanitize(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition_plain_and_encoded() {
        assert_eq!(
            from_content_disposition(r#"attachment; filename="a b.zip""#).as_deref(),
            Some("a b.zip")
        );
        assert_eq!(
            from_content_disposition(
                r#"attachment; filename="fallback.zip"; filename*=UTF-8''pel%C3%ADcula.mkv"#
            )
            .as_deref(),
            Some("película.mkv")
        );
    }

    #[test]
    fn name_from_url() {
        let u = Url::parse("https://x.org/dir/archivo%20uno.iso?token=1").unwrap();
        assert_eq!(from_url(&u).as_deref(), Some("archivo uno.iso"));
        let u = Url::parse("https://x.org/").unwrap();
        assert_eq!(choose(None, &u), "descarga");
    }

    #[test]
    fn sanitizes_for_all_platforms() {
        assert_eq!(sanitize("a:b*c?.txt"), "a_b_c_.txt");
        assert_eq!(sanitize("../../etc/passwd"), "passwd");
        assert_eq!(sanitize("..\\evil.exe"), "evil.exe");
        assert_eq!(sanitize("CON.txt"), "_CON.txt");
        assert_eq!(sanitize("com1"), "_com1");
        assert_eq!(sanitize("console.txt"), "console.txt");
        assert_eq!(sanitize("nombre. "), "nombre");
        assert_eq!(sanitize(".."), "descarga");
        let long = format!("{}.mkv", "á".repeat(300));
        let s = sanitize(&long);
        assert!(s.len() <= MAX_LEN && s.ends_with(".mkv"));
    }
}
