//! Google Drive y Google Docs.
//!
//! Los archivos públicos se bajan de `drive.usercontent.google.com/download`.
//! Si el archivo es grande, Google responde con una página de «no se puede
//! analizar en busca de virus» que lleva un formulario: se lee y se sigue.
//! Los documentos de Google (Docs, Hojas, Presentaciones) se exportan a PDF,
//! XLSX y PPTX.
//!
//! Las carpetas necesitan la API con una clave del usuario y todavía no
//! están: se avisa con claridad.

use omni_core::Error;
use omni_grab::{CheckFuture, Found};
use omni_queue::ResolveFuture;
use reqwest::header::HeaderMap;
use url::Url;

use crate::html::{attr, unescape};
use crate::{Host, Probed, direct, on_domain, probe_or_page, segments};

const NAME: &str = "Google Drive";
/// Cuántos formularios de confirmación se siguen como mucho.
const MAX_HOPS: usize = 3;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    File(String),
    /// Documento de Google: tipo (`document`, `spreadsheets`, `presentation`) e id.
    Doc(&'static str, String),
    Folder,
}

pub(crate) fn parse(url: &Url) -> Option<Kind> {
    let query_id = || {
        url.query_pairs()
            .find(|(k, _)| k == "id")
            .map(|(_, v)| v.into_owned())
    };
    let seg = segments(url);
    let seg: Vec<&str> = seg.iter().map(String::as_str).collect();
    if on_domain(url, "docs.google.com") {
        return match seg[..] {
            ["document", "d", id, ..] => Some(Kind::Doc("document", id.into())),
            ["spreadsheets", "d", id, ..] => Some(Kind::Doc("spreadsheets", id.into())),
            ["presentation", "d", id, ..] => Some(Kind::Doc("presentation", id.into())),
            ["file", "d", id, ..] => Some(Kind::File(id.into())),
            _ => None,
        };
    }
    if on_domain(url, "drive.usercontent.google.com") {
        return match seg[..] {
            ["download", ..] | ["uc", ..] => query_id().map(Kind::File),
            _ => None,
        };
    }
    if !on_domain(url, "drive.google.com") {
        return None;
    }
    match seg[..] {
        ["file", "d", id, ..] | ["file", "u", _, "d", id, ..] => Some(Kind::File(id.into())),
        ["open"] | ["uc"] => query_id().map(Kind::File),
        ["drive", "folders", ..] | ["drive", "u", _, "folders", ..] | ["folders", ..] => {
            Some(Kind::Folder)
        }
        _ => None,
    }
}

/// El formulario de confirmación de Google: su dirección con los campos
/// ocultos como parámetros.
pub(crate) fn confirm_form(html: &str, page: &Url) -> Option<Url> {
    let at = html.find("id=\"download-form\"")?;
    let start = html[..at].rfind("<form")?;
    let end = at + html[at..].find("</form>")?;
    let form = &html[start..end];
    let open_end = form.find('>')?;
    let action = attr(&form[..open_end], "action")?;
    let mut target = page.join(&unescape(&action)).ok()?;
    {
        let mut q = target.query_pairs_mut();
        q.clear();
        let mut rest = &form[open_end..];
        while let Some(i) = rest.find("<input") {
            let tag_end = rest[i..].find('>')? + i;
            let tag = &rest[i..tag_end];
            if let (Some(name), Some(value)) = (attr(tag, "name"), attr(tag, "value")) {
                q.append_pair(&name, &value);
            }
            rest = &rest[tag_end..];
        }
    }
    Some(target)
}

/// Qué dice una página de error de Google.
fn page_problem(html: &str) -> Error {
    let low = html.to_ascii_lowercase();
    Error::host(
        NAME,
        if low.contains("too many users")
            || low.contains("quota exceeded")
            || low.contains("download quota")
        {
            "cuota diaria de descargas agotada para este archivo; haz una copia en tu propio Drive o prueba mañana"
        } else if low.contains("accounts.google.com") || low.contains("sign in") {
            "el archivo es privado: hay que iniciar sesión para verlo"
        } else if low.contains("file not found") || low.contains("404") {
            return Error::LinkExpired { status: 404 };
        } else {
            "Google no dio el archivo (¿está compartido con «cualquiera con el enlace»?)"
        },
    )
}

/// Google Drive.
pub struct GoogleDrive {
    download_base: String,
    docs_base: String,
}

impl GoogleDrive {
    pub fn new() -> Self {
        Self::with_bases(
            "https://drive.usercontent.google.com",
            "https://docs.google.com",
        )
    }

    /// Con otras direcciones (pruebas).
    pub fn with_bases(download: impl Into<String>, docs: impl Into<String>) -> Self {
        Self {
            download_base: download.into(),
            docs_base: docs.into(),
        }
    }

    /// Sigue el enlace hasta dar con el archivo.
    async fn find_file(
        &self,
        client: &reqwest::Client,
        kind: Kind,
    ) -> omni_core::Result<omni_core::RemoteInfo> {
        let mut url = match kind {
            Kind::File(id) => Url::parse_with_params(
                &format!("{}/download", self.download_base.trim_end_matches('/')),
                [
                    ("id", id.as_str()),
                    ("export", "download"),
                    ("confirm", "t"),
                ],
            ),
            Kind::Doc(what, id) => {
                let format = match what {
                    "spreadsheets" => "xlsx",
                    "presentation" => "pptx",
                    _ => "pdf",
                };
                Url::parse_with_params(
                    &format!(
                        "{}/{what}/d/{id}/export",
                        self.docs_base.trim_end_matches('/')
                    ),
                    [("format", format)],
                )
            }
            Kind::Folder => {
                return Err(Error::host(
                    NAME,
                    "las carpetas de Drive aún no se pueden añadir: abre los archivos uno a uno",
                ));
            }
        }
        .map_err(|e| Error::InvalidUrl(e.to_string()))?;
        for _ in 0..MAX_HOPS {
            match probe_or_page(client, &url, &HeaderMap::new()).await? {
                Probed::File(info) => return Ok(info),
                Probed::Page { url: page, html } => match confirm_form(&html, &page) {
                    Some(next) if next != url => url = next,
                    _ => return Err(page_problem(&html)),
                },
            }
        }
        Err(Error::host(NAME, "Google pide confirmaciones sin fin"))
    }
}

impl Default for GoogleDrive {
    fn default() -> Self {
        Self::new()
    }
}

impl Host for GoogleDrive {
    fn name(&self) -> &str {
        NAME
    }

    fn handles(&self, url: &Url) -> bool {
        parse(url).is_some()
    }

    fn check<'a>(&'a self, client: &'a reqwest::Client, url: &'a Url) -> CheckFuture<'a> {
        Box::pin(async move {
            let kind = parse(url).ok_or_else(|| Error::InvalidUrl(url.to_string()))?;
            self.find_file(client, kind).await.map(Found::File)
        })
    }

    fn resolve<'a>(&'a self, client: &'a reqwest::Client, url: &'a Url) -> ResolveFuture<'a> {
        Box::pin(async move {
            let kind = parse(url).ok_or_else(|| Error::InvalidUrl(url.to_string()))?;
            Ok(direct(self.find_file(client, kind).await?.url))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(s: &str) -> Option<Kind> {
        parse(&Url::parse(s).unwrap())
    }

    #[test]
    fn recognises_links() {
        for s in [
            "https://drive.google.com/file/d/1AbC_-x/view?usp=sharing",
            "https://drive.google.com/open?id=1AbC_-x",
            "https://drive.google.com/uc?export=download&id=1AbC_-x",
            "https://drive.google.com/file/u/0/d/1AbC_-x/view",
            "https://drive.usercontent.google.com/download?id=1AbC_-x&export=download",
        ] {
            assert_eq!(kind(s), Some(Kind::File("1AbC_-x".into())), "{s}");
        }
        assert_eq!(
            kind("https://docs.google.com/spreadsheets/d/XyZ/edit#gid=0"),
            Some(Kind::Doc("spreadsheets", "XyZ".into()))
        );
        assert_eq!(
            kind("https://drive.google.com/drive/folders/1Fol?usp=sharing"),
            Some(Kind::Folder)
        );
        assert_eq!(kind("https://drive.google.com/drive/my-drive"), None);
        assert_eq!(kind("https://example.com/file/d/x"), None);
    }

    #[test]
    fn reads_the_confirmation_form() {
        let html = r#"<form id="download-form" action="https://drive.usercontent.google.com/download" method="get">
            <input type="hidden" name="id" value="ABC">
            <input type="hidden" name="export" value="download">
            <input type="hidden" name="confirm" value="t">
            <input type="hidden" name="uuid" value="u-1&amp;2">
            <input type="submit" value="Descargar de todos modos"></form>"#;
        let page = Url::parse("https://drive.usercontent.google.com/download?id=ABC").unwrap();
        let next = confirm_form(html, &page).unwrap();
        assert_eq!(
            next.as_str(),
            "https://drive.usercontent.google.com/download?id=ABC&export=download&confirm=t&uuid=u-1%262"
        );
        assert!(confirm_form("<html>nada</html>", &page).is_none());
    }
}
