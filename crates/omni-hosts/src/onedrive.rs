//! OneDrive personal: enlaces compartidos (`1drv.ms/…`, `onedrive.live.com/…`).
//!
//! La API pública de enlaces compartidos acepta el enlace codificado en
//! base64url con el prefijo `u!`:
//!
//! - `GET shares/u!ENLACE/root` → nombre, tamaño y si es archivo o carpeta.
//! - `GET shares/u!ENLACE/root/content` → el archivo (redirige a la descarga).
//! - `GET shares/u!ENLACE/root/children` → el contenido de una carpeta.
//!
//! Los enlaces de empresa (SharePoint) suelen pedir sesión y no se admiten.

use base64::Engine;
use omni_core::{Error, RemoteInfo};
use omni_grab::{CheckFuture, FolderFile, Found};
use omni_queue::ResolveFuture;
use url::Url;

use crate::{Host, direct, fetch_json, json_size, on_domain, status_error};

const NAME: &str = "OneDrive";
const MAX_FILES: usize = 5000;
/// Marca del archivo dentro de una carpeta, en el fragmento del enlace.
const ITEM: &str = "omni-item=";

fn is_share(url: &Url) -> bool {
    (on_domain(url, "1drv.ms") && url.path().len() > 1)
        || (url.host_str() == Some("onedrive.live.com")
            && matches!(url.path(), "/redir" | "/redir.aspx" | "/")
            && url.query().is_some())
        || url.host_str() == Some("onedrive.live.com") && url.path().starts_with("/embed")
}

/// El enlace sin la marca de archivo, y el id del archivo si la tenía.
fn split_item(url: &Url) -> (Url, Option<String>) {
    let item = url
        .fragment()
        .and_then(|f| f.strip_prefix(ITEM))
        .map(str::to_owned);
    let mut base = url.clone();
    if item.is_some() {
        base.set_fragment(None);
    }
    (base, item)
}

/// `u!` + el enlace en base64url sin relleno.
fn share_token(link: &Url) -> String {
    format!(
        "u!{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(link.as_str())
    )
}

/// OneDrive.
pub struct OneDrive {
    api: String,
}

impl OneDrive {
    pub fn new() -> Self {
        Self::with_api("https://api.onedrive.com/v1.0")
    }

    /// Con otra dirección para la API (pruebas).
    pub fn with_api(api: impl Into<String>) -> Self {
        Self { api: api.into() }
    }

    fn share_url(&self, link: &Url, tail: &str) -> omni_core::Result<Url> {
        Url::parse(&format!(
            "{}/shares/{}/{tail}",
            self.api.trim_end_matches('/'),
            share_token(link)
        ))
        .map_err(|e| Error::InvalidUrl(e.to_string()))
    }

    async fn get(
        &self,
        client: &reqwest::Client,
        url: Url,
    ) -> omni_core::Result<serde_json::Value> {
        let (status, json) = fetch_json(client, url).await?;
        if status.is_success() {
            return Ok(json);
        }
        let code = json["error"]["code"].as_str().unwrap_or("");
        Err(match code {
            "itemNotFound" | "invalidRequest"
                if status.as_u16() == 404 || status.as_u16() == 400 =>
            {
                Error::LinkExpired { status: 404 }
            }
            "accessDenied" | "unauthenticated" => Error::host(
                NAME,
                "el enlace es privado: hay que iniciar sesión para verlo",
            ),
            _ => status_error(status),
        })
    }

    /// Los archivos de una carpeta compartida (sin entrar en subcarpetas
    /// más allá de un nivel).
    async fn children(
        &self,
        client: &reqwest::Client,
        link: &Url,
    ) -> omni_core::Result<Vec<serde_json::Value>> {
        let mut out = Vec::new();
        let mut next = Some(self.share_url(link, "root/children?top=1000")?);
        while let Some(url) = next.take() {
            let page = self.get(client, url).await?;
            if let Some(items) = page["value"].as_array() {
                out.extend(items.iter().cloned());
            }
            next = page["@odata.nextLink"]
                .as_str()
                .and_then(|u| Url::parse(u).ok())
                .filter(|_| out.len() < MAX_FILES);
        }
        Ok(out)
    }
}

impl Default for OneDrive {
    fn default() -> Self {
        Self::new()
    }
}

impl Host for OneDrive {
    fn name(&self) -> &str {
        NAME
    }

    fn handles(&self, url: &Url) -> bool {
        is_share(url)
    }

    fn parallel(&self) -> usize {
        3
    }

    fn check<'a>(&'a self, client: &'a reqwest::Client, url: &'a Url) -> CheckFuture<'a> {
        Box::pin(async move {
            let (link, item) = split_item(url);
            let root = self.get(client, self.share_url(&link, "root")?).await?;
            if root["folder"].is_object() {
                let files: Vec<FolderFile> = self
                    .children(client, &link)
                    .await?
                    .iter()
                    .filter(|c| c["file"].is_object())
                    .filter(|c| item.as_deref().is_none_or(|i| c["id"] == i))
                    .filter_map(|c| {
                        let mut u = link.clone();
                        u.set_fragment(Some(&format!("{ITEM}{}", c["id"].as_str()?)));
                        Some(FolderFile {
                            url: u,
                            name: c["name"].as_str()?.to_owned(),
                            size: json_size(&c["size"]),
                            resumable: true,
                        })
                    })
                    .collect();
                if files.is_empty() {
                    return Err(Error::host(NAME, "la carpeta no tiene archivos"));
                }
                return Ok(Found::Folder {
                    name: root["name"].as_str().map(str::to_owned),
                    files,
                });
            }
            // Un archivo suelto (o uno de una carpeta, si trae su marca).
            let node = match &item {
                Some(id) => self
                    .children(client, &link)
                    .await?
                    .into_iter()
                    .find(|c| c["id"] == id.as_str())
                    .ok_or(Error::LinkExpired { status: 404 })?,
                None => root,
            };
            Ok(Found::File(RemoteInfo {
                url: self.share_url(&link, "root/content")?,
                size: json_size(&node["size"]),
                accepts_ranges: true,
                etag: None,
                last_modified: None,
                filename: omni_core::filename::sanitize(node["name"].as_str().unwrap_or("archivo")),
            }))
        })
    }

    fn resolve<'a>(&'a self, client: &'a reqwest::Client, url: &'a Url) -> ResolveFuture<'a> {
        Box::pin(async move {
            let (link, item) = split_item(url);
            let Some(id) = item else {
                return Ok(direct(self.share_url(&link, "root/content")?));
            };
            // Un archivo de una carpeta: el enlace directo se pide ahora
            // porque caduca en una hora.
            let child = self
                .children(client, &link)
                .await?
                .into_iter()
                .find(|c| c["id"] == id.as_str())
                .ok_or(Error::LinkExpired { status: 404 })?;
            let download = child["@content.downloadUrl"]
                .as_str()
                .ok_or_else(|| Error::host(NAME, "OneDrive no dio el enlace de descarga"))?;
            Ok(direct(download))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_share_links() {
        let s = |u: &str| is_share(&Url::parse(u).unwrap());
        assert!(s("https://1drv.ms/u/s!AbCdEf"));
        assert!(s("https://1drv.ms/f/c/abc/def"));
        assert!(s("https://onedrive.live.com/redir?resid=ABC&authkey=!x"));
        assert!(!s("https://1drv.ms/"));
        assert!(!s("https://onedrive.live.com/about/es-es/"));
        assert!(!s("https://example.com/redir?a=b"));
    }

    #[test]
    fn share_token_is_url_safe_base64() {
        let t = share_token(&Url::parse("https://1drv.ms/u/s!AbC").unwrap());
        assert!(t.starts_with("u!aHR0cHM6Ly8x"));
        assert!(!t.contains(['=', '+', '/']));
    }

    #[test]
    fn splits_the_item_mark() {
        let (base, item) = split_item(&Url::parse("https://1drv.ms/f/s!A#omni-item=ID1").unwrap());
        assert_eq!(base.as_str(), "https://1drv.ms/f/s!A");
        assert_eq!(item.as_deref(), Some("ID1"));
        assert_eq!(
            split_item(&Url::parse("https://1drv.ms/f/s!A").unwrap()).1,
            None
        );
    }
}
