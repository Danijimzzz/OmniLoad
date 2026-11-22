//! Lo mínimo para leer páginas de hosts sin un analizador HTML entero.

/// La etiqueta de apertura (`<a ...>`) que contiene `needle`, sin los `<>`.
pub fn tag_containing<'a>(html: &'a str, needle: &str) -> Option<&'a str> {
    let at = html.find(needle)?;
    let start = html[..at].rfind('<')?;
    let end = at + html[at..].find('>')?;
    Some(&html[start + 1..end])
}

/// El valor del atributo `name` de una etiqueta (con comillas dobles o simples).
pub fn attr(tag: &str, name: &str) -> Option<String> {
    let mut from = 0;
    while let Some(i) = tag[from..].find(name) {
        let at = from + i;
        from = at + name.len();
        let before_ok = at == 0 || tag.as_bytes()[at - 1].is_ascii_whitespace();
        let rest = tag[at + name.len()..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        if !before_ok {
            continue;
        }
        let rest = rest.trim_start();
        let quote = rest.chars().next()?;
        if quote != '"' && quote != '\'' {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            return Some(unescape(&rest[..end]));
        }
        let body = &rest[1..];
        let end = body.find(quote)?;
        return Some(unescape(&body[..end]));
    }
    None
}

/// Entidades HTML que aparecen en enlaces.
pub fn unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&#38;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x2F;", "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_attributes() {
        let html = r#"<p><a class="input" id="downloadButton" href="https://x.com/a?b=1&amp;c=2" data-x='y'>Go</a>"#;
        let tag = tag_containing(html, r#"id="downloadButton""#).unwrap();
        assert_eq!(attr(tag, "href").unwrap(), "https://x.com/a?b=1&c=2");
        assert_eq!(attr(tag, "data-x").unwrap(), "y");
        assert_eq!(attr(tag, "id").unwrap(), "downloadButton");
        assert!(attr(tag, "rel").is_none());
        // "ref" no es "href".
        assert!(attr("a href=\"1\"", "ref").is_none());
    }
}
