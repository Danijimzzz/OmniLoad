//! Mediafire.
//!
//! - **Archivo:** se lee la página (`/file/ID/nombre/file`) y se saca el
//!   enlace del botón de descarga (`#downloadButton`), que viene en `href` o,
//!   en las páginas nuevas, ofuscado en base64 en `data-scrambled-url`.
//! - **Carpeta:** API pública `api/1.5/folder/get_content.php`.
//!
//! El HTML de Mediafire cambia de vez en cuando; todo lo que depende de él
//! está en [`download_link`], que es lo que habrá que tocar si deja de ir.

use base64::Engine;
use omni_core::Error;
use omni_grab::{CheckFuture, FolderFile, Found};
use omni_queue::ResolveFuture;
use url::Url;

use crate::html::{attr, tag_containing};
use crate::{Host, direct, fetch_json, fetch_text, json_size, on_domain, segments, status_error};

const NAME: &str = "Mediafire";
/// Carpetas dentro de carpetas que se recorren, y archivos como máximo.
const MAX_DEPTH: usize = 3;
const MAX_FILES: usize = 5000;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Clave del archivo.
    File(String),
    /// Clave de la carpeta.
    Folder(String),
}

pub(crate) fn parse(url: &Url) -> Option<Kind> {
    if !on_domain(url, "mediafire.com") {
        return None;
    }
    // `download123.mediafire.com` es el servidor de datos: ya es directo.
    if url.host_str().is_some_and(|h| h.starts_with("download")) {
        return None;
    }
    let seg = segments(url);
    match seg.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["file" | "file_premium" | "view" | "download", key, ..] => {
            Some(Kind::File(key.to_owned()))
        }
        ["folder", key, ..] => Some(Kind::Folder(key.to_owned())),
        // Forma antigua: `mediafire.com/?clavedelarchivo`.
        [] => {
            let q = url.query()?;
            let key = q.split('&').next()?;
            (!key.is_empty() && !key.contains('=')).then(|| Kind::File(key.to_owned()))
        }
        _ => None,
    }
}

/// El enlace directo que lleva la página de un archivo.
pub(crate) fn download_link(html: &str) -> Option<String> {
    if let Some(tag) = tag_containing(html, "id=\"downloadButton\"") {
        if let Some(scrambled) = attr(tag, "data-scrambled-url")
            && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(scrambled.trim())
            && let Ok(url) = String::from_utf8(bytes)
            && url.starts_with("http")
        {
            return Some(url);
        }
        if let Some(href) = attr(tag, "href").filter(|h| h.starts_with("http")) {
            return Some(href);
        }
    }
    // Plan B: cualquier enlace al servidor de datos de la página.
    let at = html.find("https://download")?;
    let rest = &html[at..];
    let end = rest
        .find(|c: char| c == '"' || c == '\'' || c == '<' || c.is_whitespace())
        .unwrap_or(rest.len());
    let url = crate::html::unescape(&rest[..end]);
    Url::parse(&url)
        .ok()
        .filter(|u| u.host_str().is_some_and(|h| h.ends_with("mediafire.com")))
        .map(|_| url)
}

/// Mediafire.
pub struct Mediafire {
    base: String,
}

impl Mediafire {
    pub fn new() -> Self {
        Self::with_base("https://www.mediafire.com")
    }

    /// Con otra dirección (pruebas).
    pub fn with_base(base: impl Into<String>) -> Self {
        Self { base: base.into() }
    }

    fn url(&self, path_and_query: &str) -> omni_core::Result<Url> {
        Url::parse(&format!(
            "{}/{path_and_query}",
            self.base.trim_end_matches('/')
        ))
        .map_err(|e| Error::InvalidUrl(e.to_string()))
    }

    /// Lee la página del archivo y devuelve su enlace directo.
    async fn file_link(&self, client: &reqwest::Client, key: &str) -> omni_core::Result<Url> {
        let (status, html) = fetch_text(client, self.url(&format!("file/{key}/file"))?).await?;
        if status.as_u16() == 404 {
            return Err(Error::LinkExpired { status: 404 });
        }
        if !status.is_success() {
            return Err(status_error(status));
        }
        match download_link(&html) {
            Some(link) => Url::parse(&link).map_err(|e| Error::InvalidUrl(e.to_string())),
            None => Err(Error::host(
                NAME,
                if html.contains("g-recaptcha") || html.contains("captcha") {
                    "pide un captcha: ábrelo en el navegador"
                } else if html.contains("key you provided")
                    || html.contains("File Removed")
                    || html.contains("has been deleted")
                {
                    "el archivo ya no existe"
                } else if html.contains("password") {
                    "el archivo tiene contraseña"
                } else {
                    "no se encontró el enlace de descarga (¿Mediafire cambió su página?)"
                },
            )),
        }
    }

    /// Llama a la API y da su `response`, o el error que diga.
    async fn api(
        &self,
        path_and_query: &str,
        client: &reqwest::Client,
    ) -> omni_core::Result<serde_json::Value> {
        let (status, json) = fetch_json(
            client,
            self.url(&format!("api/1.5/{path_and_query}&response_format=json"))?,
        )
        .await?;
        if !status.is_success() {
            return Err(status_error(status));
        }
        let resp = &json["response"];
        if resp["result"] == "Error" {
            let msg = resp["message"].as_str().unwrap_or("error desconocido");
            let low = msg.to_ascii_lowercase();
            return Err(if low.contains("key") || low.contains("not found") {
                Error::LinkExpired { status: 404 }
            } else {
                Error::host(NAME, msg.to_owned())
            });
        }
        Ok(resp.clone())
    }

    async fn folder_files(
        &self,
        client: &reqwest::Client,
        key: &str,
        depth: usize,
        out: &mut Vec<FolderFile>,
    ) -> omni_core::Result<()> {
        let mut chunk = 1;
        loop {
            let resp = self
                .api(
                    &format!(
                        "folder/get_content.php?folder_key={key}&content_type=files&chunk={chunk}&chunk_size=1000"
                    ),
                    client,
                )
                .await?;
            let content = &resp["folder_content"];
            for f in content["files"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
            {
                let (Some(qk), Some(name)) = (f["quickkey"].as_str(), f["filename"].as_str())
                else {
                    continue;
                };
                let mut u = self.url("file/")?;
                u.path_segments_mut()
                    .map_err(|_| Error::InvalidUrl("base".into()))?
                    .pop_if_empty()
                    .push(qk)
                    .push(name)
                    .push("file");
                out.push(FolderFile {
                    url: u,
                    name: name.to_owned(),
                    size: json_size(&f["size"]),
                    resumable: true,
                });
                if out.len() >= MAX_FILES {
                    return Ok(());
                }
            }
            if content["more_chunks"] != "yes" {
                break;
            }
            chunk += 1;
        }
        if depth < MAX_DEPTH {
            let resp = self
                .api(
                    &format!("folder/get_content.php?folder_key={key}&content_type=folders&chunk_size=1000"),
                    client,
                )
                .await?;
            let subs: Vec<String> = resp["folder_content"]["folders"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter_map(|f| f["folderkey"].as_str().map(str::to_owned))
                .collect();
            for sub in subs {
                if out.len() >= MAX_FILES {
                    break;
                }
                Box::pin(self.folder_files(client, &sub, depth + 1, out)).await?;
            }
        }
        Ok(())
    }
}

impl Default for Mediafire {
    fn default() -> Self {
        Self::new()
    }
}

impl Host for Mediafire {
    fn name(&self) -> &str {
        NAME
    }

    fn handles(&self, url: &Url) -> bool {
        parse(url).is_some()
    }

    fn parallel(&self) -> usize {
        3
    }

    fn check<'a>(&'a self, client: &'a reqwest::Client, url: &'a Url) -> CheckFuture<'a> {
        Box::pin(async move {
            match parse(url).ok_or_else(|| Error::InvalidUrl(url.to_string()))? {
                Kind::File(key) => {
                    let link = self.file_link(client, &key).await?;
                    omni_core::probe(client, &link).await.map(Found::File)
                }
                Kind::Folder(key) => {
                    let mut files = Vec::new();
                    self.folder_files(client, &key, 1, &mut files).await?;
                    if files.is_empty() {
                        return Err(Error::host(NAME, "la carpeta está vacía"));
                    }
                    let name = self
                        .api(&format!("folder/get_info.php?folder_key={key}"), client)
                        .await
                        .ok()
                        .and_then(|r| r["folder_info"]["name"].as_str().map(str::to_owned));
                    Ok(Found::Folder { name, files })
                }
            }
        })
    }

    fn resolve<'a>(&'a self, client: &'a reqwest::Client, url: &'a Url) -> ResolveFuture<'a> {
        Box::pin(async move {
            match parse(url).ok_or_else(|| Error::InvalidUrl(url.to_string()))? {
                Kind::File(key) => {
                    let link = self.file_link(client, &key).await?;
                    Ok(direct(link))
                }
                Kind::Folder(_) => Err(Error::host(
                    NAME,
                    "es una carpeta: hay que añadir sus archivos uno a uno",
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
            kind("https://www.mediafire.com/file/abc123xyz/mi_archivo.zip/file"),
            Some(Kind::File("abc123xyz".into()))
        );
        assert_eq!(
            kind("http://www.mediafire.com/file/abc123xyz"),
            Some(Kind::File("abc123xyz".into()))
        );
        assert_eq!(
            kind("https://www.mediafire.com/?abc123xyz"),
            Some(Kind::File("abc123xyz".into()))
        );
        assert_eq!(
            kind("https://www.mediafire.com/folder/k1/Mi_Carpeta"),
            Some(Kind::Folder("k1".into()))
        );
        assert_eq!(kind("https://download1234.mediafire.com/x/abc/a.zip"), None);
        assert_eq!(kind("https://www.mediafire.com/"), None);
        assert_eq!(kind("https://www.mediafire.com/software/"), None);
    }

    #[test]
    fn finds_the_download_link() {
        let plain = r#"<a class="input popsok" href="https://download1.mediafire.com/abc/k/a%20b.zip" id="downloadButton">Download</a>"#;
        assert_eq!(
            download_link(plain).unwrap(),
            "https://download1.mediafire.com/abc/k/a%20b.zip"
        );
        let scrambled = format!(
            r##"<a id="downloadButton" href="#" data-scrambled-url="{}" class="x">"##,
            base64::engine::general_purpose::STANDARD
                .encode("https://download2.mediafire.com/z/k/f.rar")
        );
        assert_eq!(
            download_link(&scrambled).unwrap(),
            "https://download2.mediafire.com/z/k/f.rar"
        );
        let fallback = r#"<script>var u="https://download3.mediafire.com/q/k/f.7z";</script>"#;
        assert_eq!(
            download_link(fallback).unwrap(),
            "https://download3.mediafire.com/q/k/f.7z"
        );
        assert!(download_link("<html>nada</html>").is_none());
        // Un enlace a otro dominio no vale.
        assert!(download_link(r#"<a href="https://download.evil.com/x">"#).is_none());
    }
}
