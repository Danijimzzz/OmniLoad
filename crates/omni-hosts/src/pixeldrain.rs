//! Pixeldrain: API oficial y pública (<https://pixeldrain.com/api>).
//!
//! - `GET /api/file/ID/info`: nombre y tamaño de un archivo.
//! - `GET /api/file/ID`: el archivo (acepta `Range`).
//! - `GET /api/list/ID`: los archivos de una lista (carpeta).

use omni_core::{Error, RemoteInfo};
use omni_grab::{CheckFuture, FolderFile, Found};
use omni_queue::ResolveFuture;
use reqwest::StatusCode;
use url::Url;

use crate::{Host, direct, fetch_json, json_size, on_domain, segments, status_error};

const NAME: &str = "Pixeldrain";
const DOMAINS: [&str; 3] = ["pixeldrain.com", "pixeldrain.net", "pixeldrain.dev"];

/// Qué enlace de Pixeldrain es.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    File(String),
    List(String),
}

pub(crate) fn parse(url: &Url) -> Option<Kind> {
    if !DOMAINS.iter().any(|d| on_domain(url, d)) {
        return None;
    }
    let seg = segments(url);
    match seg.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["u", id] | ["d", id] | ["api", "file", id] => Some(Kind::File(id.to_owned())),
        ["api", "file", id, "info"] => Some(Kind::File(id.to_owned())),
        ["l", id] | ["api", "list", id] => Some(Kind::List(id.to_owned())),
        _ => None,
    }
}

/// Pixeldrain.
pub struct Pixeldrain {
    base: String,
}

impl Pixeldrain {
    pub fn new() -> Self {
        Self::with_base("https://pixeldrain.com")
    }

    /// Con otra dirección para la API (pruebas).
    pub fn with_base(base: impl Into<String>) -> Self {
        Self { base: base.into() }
    }

    fn api(&self, path: &str) -> omni_core::Result<Url> {
        Url::parse(&format!("{}/api/{path}", self.base.trim_end_matches('/')))
            .map_err(|e| Error::InvalidUrl(e.to_string()))
    }
}

impl Default for Pixeldrain {
    fn default() -> Self {
        Self::new()
    }
}

/// Un error de la API (`{"success":false,"value":"file_not_found",...}`).
fn api_error(status: StatusCode, json: &serde_json::Value) -> Error {
    let value = json["value"].as_str().unwrap_or("");
    match value {
        "file_not_found" | "list_not_found" => Error::LinkExpired { status: 404 },
        v if v.contains("captcha") => Error::host(
            NAME,
            "pide un captcha (demasiadas descargas): ábrelo en el navegador",
        ),
        v if v.contains("rate_limit") => {
            Error::host(NAME, "límite de descargas alcanzado; prueba más tarde")
        }
        v if v.contains("virus") => Error::host(NAME, "bloqueado por sospecha de virus"),
        "" => status_error(status),
        v => Error::host(NAME, v.replace('_', " ")),
    }
}

impl Host for Pixeldrain {
    fn name(&self) -> &str {
        NAME
    }

    fn handles(&self, url: &Url) -> bool {
        parse(url).is_some()
    }

    fn check<'a>(&'a self, client: &'a reqwest::Client, url: &'a Url) -> CheckFuture<'a> {
        Box::pin(async move {
            match parse(url).ok_or_else(|| Error::InvalidUrl(url.to_string()))? {
                Kind::File(id) => {
                    let (status, json) =
                        fetch_json(client, self.api(&format!("file/{id}/info"))?).await?;
                    if !status.is_success() || json["success"] == false {
                        return Err(api_error(status, &json));
                    }
                    Ok(Found::File(RemoteInfo {
                        url: self.api(&format!("file/{id}"))?,
                        size: json_size(&json["size"]),
                        accepts_ranges: true,
                        etag: None,
                        last_modified: None,
                        filename: omni_core::filename::sanitize(
                            json["name"].as_str().unwrap_or(&id),
                        ),
                    }))
                }
                Kind::List(id) => {
                    let (status, json) =
                        fetch_json(client, self.api(&format!("list/{id}"))?).await?;
                    if !status.is_success() || json["success"] == false {
                        return Err(api_error(status, &json));
                    }
                    let files: Vec<FolderFile> = json["files"]
                        .as_array()
                        .map(Vec::as_slice)
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|f| {
                            let fid = f["id"].as_str()?;
                            Some(FolderFile {
                                url: Url::parse(&format!("https://pixeldrain.com/u/{fid}")).ok()?,
                                name: f["name"].as_str().unwrap_or(fid).to_owned(),
                                size: json_size(&f["size"]),
                                resumable: true,
                            })
                        })
                        .collect();
                    if files.is_empty() {
                        return Err(Error::host(NAME, "la lista no tiene archivos"));
                    }
                    Ok(Found::Folder {
                        name: json["title"]
                            .as_str()
                            .filter(|t| !t.is_empty())
                            .map(str::to_owned),
                        files,
                    })
                }
            }
        })
    }

    fn resolve<'a>(&'a self, _client: &'a reqwest::Client, url: &'a Url) -> ResolveFuture<'a> {
        Box::pin(async move {
            match parse(url).ok_or_else(|| Error::InvalidUrl(url.to_string()))? {
                Kind::File(id) => Ok(direct(self.api(&format!("file/{id}"))?)),
                Kind::List(_) => Err(Error::host(
                    NAME,
                    "es una lista: hay que añadir sus archivos uno a uno",
                )),
            }
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
        assert_eq!(
            kind("https://pixeldrain.com/u/AbC123"),
            Some(Kind::File("AbC123".into()))
        );
        assert_eq!(
            kind("https://www.pixeldrain.net/u/x?download"),
            Some(Kind::File("x".into()))
        );
        assert_eq!(
            kind("https://pixeldrain.com/api/file/Zz"),
            Some(Kind::File("Zz".into()))
        );
        assert_eq!(
            kind("https://pixeldrain.com/l/Lst1#item=2"),
            Some(Kind::List("Lst1".into()))
        );
        assert_eq!(kind("https://pixeldrain.com/"), None);
        assert_eq!(kind("https://pixeldrain.com/user/files"), None);
        assert_eq!(kind("https://notpixeldrain.com/u/x"), None);
    }
}
