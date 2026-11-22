//! Archive.org: API oficial de metadatos.
//!
//! `GET /metadata/ID` lista todos los archivos de un elemento con su tamaño y
//! sus sumas; cada archivo se baja de la dirección fija
//! `/download/ID/ruta/archivo` (acepta `Range`).

use omni_core::Error;
use omni_grab::{CheckFuture, FolderFile, Found};
use omni_queue::ResolveFuture;
use url::Url;

use crate::{Host, fetch_json, json_size, on_domain, segments, status_error};

const NAME: &str = "Archive.org";

/// El identificador del elemento si el enlace es de su página
/// (`/details/ID`) o de su carpeta de descarga (`/download/ID`).
pub(crate) fn item_id(url: &Url) -> Option<String> {
    if !on_domain(url, "archive.org") {
        return None;
    }
    let seg = segments(url);
    match seg.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["details", id, ..] | ["download", id] | ["metadata", id] => Some(id.to_owned()),
        _ => None,
    }
}

/// Archive.org.
pub struct ArchiveOrg {
    base: String,
}

impl ArchiveOrg {
    pub fn new() -> Self {
        Self::with_base("https://archive.org")
    }

    /// Con otra dirección (pruebas).
    pub fn with_base(base: impl Into<String>) -> Self {
        Self { base: base.into() }
    }
}

impl Default for ArchiveOrg {
    fn default() -> Self {
        Self::new()
    }
}

impl Host for ArchiveOrg {
    fn name(&self) -> &str {
        NAME
    }

    fn handles(&self, url: &Url) -> bool {
        item_id(url).is_some()
    }

    fn check<'a>(&'a self, client: &'a reqwest::Client, url: &'a Url) -> CheckFuture<'a> {
        Box::pin(async move {
            let id = item_id(url).ok_or_else(|| Error::InvalidUrl(url.to_string()))?;
            let base = self.base.trim_end_matches('/');
            let meta = Url::parse(&format!("{base}/metadata/"))
                .and_then(|u| u.join(&id))
                .map_err(|e| Error::InvalidUrl(e.to_string()))?;
            let (status, json) = fetch_json(client, meta).await?;
            if !status.is_success() {
                return Err(status_error(status));
            }
            // Un elemento que no existe devuelve `{}`.
            if json.get("files").is_none() {
                return Err(Error::LinkExpired { status: 404 });
            }
            if json["is_dark"] == true {
                return Err(Error::host(NAME, "el elemento ha sido retirado"));
            }
            let files: Vec<FolderFile> = json["files"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter(|f| f["source"] != "metadata" && f["private"] != "true")
                .filter_map(|f| {
                    let path = f["name"].as_str()?;
                    let mut file_url = Url::parse(&format!("{base}/download/")).ok()?;
                    {
                        let mut segs = file_url.path_segments_mut().ok()?;
                        segs.pop_if_empty().push(&id);
                        for part in path.split('/') {
                            segs.push(part);
                        }
                    }
                    Some(FolderFile {
                        url: file_url,
                        name: path.rsplit('/').next().unwrap_or(path).to_owned(),
                        size: json_size(&f["size"]),
                        resumable: true,
                    })
                })
                .collect();
            if files.is_empty() {
                return Err(Error::host(
                    NAME,
                    "este elemento no tiene archivos descargables (puede ser de acceso restringido)",
                ));
            }
            Ok(Found::Folder {
                name: json["metadata"]["title"]
                    .as_str()
                    .filter(|t| !t.is_empty())
                    .map(str::to_owned)
                    .or(Some(id)),
                files,
            })
        })
    }

    /// Los archivos de Archive.org ya son enlaces directos
    /// (`/download/ID/archivo`); solo la página del elemento no lo es.
    fn resolve<'a>(&'a self, _client: &'a reqwest::Client, url: &'a Url) -> ResolveFuture<'a> {
        Box::pin(async move {
            Err(Error::host(
                NAME,
                format!("{url} es una página: hay que elegir sus archivos"),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> Option<String> {
        item_id(&Url::parse(s).unwrap())
    }

    #[test]
    fn recognises_links() {
        assert_eq!(
            id("https://archive.org/details/ubuntu-16.04").as_deref(),
            Some("ubuntu-16.04")
        );
        assert_eq!(
            id("https://www.archive.org/details/x/y.mp4?q=1").as_deref(),
            Some("x")
        );
        assert_eq!(
            id("https://archive.org/download/foo").as_deref(),
            Some("foo")
        );
        // Un archivo concreto ya es un enlace directo.
        assert_eq!(id("https://archive.org/download/foo/a.zip"), None);
        assert_eq!(id("https://archive.org/search?query=x"), None);
        assert_eq!(id("https://example.com/details/foo"), None);
    }
}
