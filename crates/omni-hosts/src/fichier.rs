//! 1fichier, solo en modo gratuito (sin cuenta ni API premium).
//!
//! La descarga gratuita es una conversación con la página: se pide el
//! archivo, se envía el formulario y 1fichier contesta con el enlace o con
//! «espera N minutos». Una sola descarga a la vez y con esperas entre una y
//! otra: la resolución espera lo que pida (se puede cancelar) y sigue.
//!
//! Lo que depende del HTML de 1fichier está en [`parse_page`],
//! [`wait_minutes`] y [`download_link`].

use std::time::Duration;

use omni_core::{Error, RemoteInfo};
use omni_grab::{CheckFuture, FolderFile, Found};
use omni_queue::ResolveFuture;
use url::Url;

use crate::html::{attr, unescape};
use crate::{Host, direct, fetch_json, fetch_text, on_domain, segments, status_error};

const NAME: &str = "1fichier";
/// Lo máximo que se espera a que 1fichier deje descargar.
const MAX_WAIT: Duration = Duration::from_secs(20 * 60);
const MAX_ATTEMPTS: usize = 3;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    File(String),
    Folder(String),
}

pub(crate) fn parse(url: &Url) -> Option<Kind> {
    if !on_domain(url, "1fichier.com") {
        return None;
    }
    let seg = segments(url);
    if let ["dir", id] = seg.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        return Some(Kind::Folder(id.to_owned()));
    }
    // `ID.1fichier.com`
    let host = url.host_str()?.to_ascii_lowercase();
    if let Some(sub) = host.strip_suffix(".1fichier.com")
        && sub != "www"
        && sub.len() >= 8
        && sub.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return Some(Kind::File(sub.to_owned()));
    }
    // `1fichier.com/?ID`
    let key = url.query()?.split('&').next()?;
    (seg.is_empty() && key.len() >= 8 && key.chars().all(|c| c.is_ascii_alphanumeric()))
        .then(|| Kind::File(key.to_owned()))
}

/// Lo que dice la página de un archivo.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Page {
    pub name: Option<String>,
    pub size: Option<u64>,
    /// Valor oculto `adz` del formulario.
    pub adz: Option<String>,
}

/// El texto de la celda que sigue a una etiqueta («Filename :», «Size :»).
fn cell_after(html: &str, label: &str) -> Option<String> {
    let at = html.find(label)?;
    let rest = &html[at..];
    let start = rest.find("class=\"normal\">")? + "class=\"normal\">".len();
    let end = rest[start..].find("</td>")?;
    let text = unescape(rest[start..start + end].trim());
    (!text.is_empty()).then_some(text)
}

/// «1.5 GB» → bytes.
fn parse_size(text: &str) -> Option<u64> {
    let mut it = text.split_whitespace();
    let number: f64 = it.next()?.replace(',', ".").parse().ok()?;
    let mult: f64 = match it.next()?.to_ascii_uppercase().as_str() {
        "B" | "O" => 1.0,
        "KB" | "KO" => 1e3,
        "MB" | "MO" => 1e6,
        "GB" | "GO" => 1e9,
        "TB" | "TO" => 1e12,
        _ => return None,
    };
    Some((number * mult) as u64)
}

pub(crate) fn parse_page(html: &str) -> Page {
    let adz = html.find("name=\"adz\"").and_then(|at| {
        let start = html[..at].rfind('<')?;
        let end = at + html[at..].find('>')?;
        attr(&html[start..end], "value")
    });
    Page {
        name: cell_after(html, "Filename").or_else(|| cell_after(html, "Nom du fichier")),
        size: cell_after(html, "Size")
            .or_else(|| cell_after(html, "Taille"))
            .and_then(|s| parse_size(&s)),
        adz,
    }
}

fn is_gone(html: &str) -> bool {
    let low = html.to_ascii_lowercase();
    low.contains("could not be found")
        || low.contains("has been deleted")
        || low.contains("file not found")
        || low.contains("introuvable")
}

/// Minutos que pide esperar 1fichier («You must wait 5 minutes»).
pub(crate) fn wait_minutes(html: &str) -> Option<u64> {
    let low = html.to_ascii_lowercase();
    let at = low
        .find("you must wait")
        .or_else(|| low.find("vous devez attendre"))?;
    let digits: String = low[at..]
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// El enlace de descarga de la página de después del formulario.
pub(crate) fn download_link(html: &str) -> Option<String> {
    let mut rest = html;
    while let Some(i) = rest.find("<a ") {
        let end = rest[i..].find('>')? + i;
        let tag = &rest[i..end];
        if let Some(href) = attr(tag, "href")
            && let Ok(u) = Url::parse(&href)
            && u.scheme() == "https"
            && u.host_str().is_some_and(|h| h.ends_with(".1fichier.com"))
            && u.host_str() != Some("www.1fichier.com")
            && u.path().len() > 3
        {
            return Some(href);
        }
        rest = &rest[end..];
    }
    None
}

/// 1fichier.
pub struct OneFichier {
    base: String,
    /// Multiplicador de las esperas (las pruebas lo ponen a cero).
    wait_scale: f64,
}

impl OneFichier {
    pub fn new() -> Self {
        Self::with_base("https://1fichier.com")
    }

    /// Con otra dirección (pruebas).
    pub fn with_base(base: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            wait_scale: 1.0,
        }
    }

    /// Sin esperas de verdad (pruebas).
    pub fn without_waits(mut self) -> Self {
        self.wait_scale = 0.0;
        self
    }

    fn page_url(&self, id: &str) -> omni_core::Result<Url> {
        Url::parse(&format!("{}/?{id}", self.base.trim_end_matches('/')))
            .map_err(|e| Error::InvalidUrl(e.to_string()))
    }
}

impl Default for OneFichier {
    fn default() -> Self {
        Self::new()
    }
}

impl Host for OneFichier {
    fn name(&self) -> &str {
        NAME
    }

    fn handles(&self, url: &Url) -> bool {
        parse(url).is_some()
    }

    fn parallel(&self) -> usize {
        1
    }

    fn check<'a>(&'a self, client: &'a reqwest::Client, url: &'a Url) -> CheckFuture<'a> {
        Box::pin(async move {
            match parse(url).ok_or_else(|| Error::InvalidUrl(url.to_string()))? {
                Kind::File(id) => {
                    let (status, html) = fetch_text(client, self.page_url(&id)?).await?;
                    if is_gone(&html) || status.as_u16() == 404 {
                        return Err(Error::LinkExpired { status: 404 });
                    }
                    if !status.is_success() {
                        return Err(status_error(status));
                    }
                    let page = parse_page(&html);
                    let Some(name) = page.name else {
                        return Err(Error::host(
                            NAME,
                            "no se pudo leer la página (¿1fichier cambió su diseño?)",
                        ));
                    };
                    Ok(Found::File(RemoteInfo {
                        url: url.clone(),
                        size: page.size,
                        accepts_ranges: false,
                        etag: None,
                        last_modified: None,
                        filename: omni_core::filename::sanitize(&name),
                    }))
                }
                Kind::Folder(id) => {
                    let dir = Url::parse(&format!(
                        "{}/dir/{id}?json=1",
                        self.base.trim_end_matches('/')
                    ))
                    .map_err(|e| Error::InvalidUrl(e.to_string()))?;
                    let (status, json) = fetch_json(client, dir).await?;
                    if !status.is_success() {
                        return Err(status_error(status));
                    }
                    let files: Vec<FolderFile> = json
                        .as_array()
                        .map(Vec::as_slice)
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|f| {
                            Some(FolderFile {
                                url: Url::parse(f["url"].as_str()?).ok()?,
                                name: f["filename"].as_str()?.to_owned(),
                                size: crate::json_size(&f["size"]),
                                resumable: false,
                            })
                        })
                        .collect();
                    if files.is_empty() {
                        return Err(Error::host(
                            NAME,
                            "no se pudo leer la carpeta (vacía, privada o con otro formato)",
                        ));
                    }
                    Ok(Found::Folder { name: None, files })
                }
            }
        })
    }

    fn resolve<'a>(&'a self, client: &'a reqwest::Client, url: &'a Url) -> ResolveFuture<'a> {
        Box::pin(async move {
            let Some(Kind::File(id)) = parse(url) else {
                return Err(Error::host(NAME, "es una carpeta: añade sus archivos"));
            };
            let page_url = self.page_url(&id)?;
            for attempt in 0..MAX_ATTEMPTS {
                let (status, html) = fetch_text(client, page_url.clone()).await?;
                if is_gone(&html) || status.as_u16() == 404 {
                    return Err(Error::LinkExpired { status: 404 });
                }
                let Some(adz) = parse_page(&html).adz else {
                    // Sin formulario puede ser ya la espera.
                    if let Some(min) = wait_minutes(&html) {
                        self.wait(min, attempt).await?;
                        continue;
                    }
                    return Err(Error::host(
                        NAME,
                        "no se encontró el formulario de descarga (¿1fichier cambió su diseño?)",
                    ));
                };
                let resp = client
                    .post(page_url.clone())
                    .form(&[("dl_no_ssl", "on"), ("adz", adz.as_str())])
                    .send()
                    .await?;
                let html = resp.text().await?;
                if let Some(link) = download_link(&html) {
                    return Ok(direct(link));
                }
                if let Some(min) = wait_minutes(&html) {
                    self.wait(min, attempt).await?;
                    continue;
                }
                let low = html.to_ascii_lowercase();
                return Err(Error::host(
                    NAME,
                    if low.contains("captcha") || low.contains("cloudflare") {
                        "pide una comprobación en el navegador: ábrelo allí una vez"
                    } else if low.contains("only download one file at a time")
                        || low.contains("un seul fichier")
                    {
                        "solo se puede bajar un archivo a la vez sin cuenta"
                    } else {
                        "no se obtuvo el enlace de descarga"
                    },
                ));
            }
            Err(Error::host(
                NAME,
                "sigue pidiendo esperar: prueba de nuevo en un rato",
            ))
        })
    }
}

impl OneFichier {
    /// Espera lo que pide 1fichier (+2 s), si no es demasiado.
    async fn wait(&self, minutes: u64, attempt: usize) -> omni_core::Result<()> {
        let wait = Duration::from_secs(minutes * 60 + 2);
        if wait > MAX_WAIT || attempt + 1 >= MAX_ATTEMPTS {
            return Err(Error::host(
                NAME,
                format!(
                    "1fichier pide esperar {minutes} min entre descargas gratuitas; prueba más tarde"
                ),
            ));
        }
        tokio::time::sleep(wait.mul_f64(self.wait_scale)).await;
        Ok(())
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
            kind("https://1fichier.com/?abcdefghij1234567890"),
            Some(Kind::File("abcdefghij1234567890".into()))
        );
        assert_eq!(
            kind("https://abcdefghij1234567890.1fichier.com/"),
            Some(Kind::File("abcdefghij1234567890".into()))
        );
        assert_eq!(
            kind("https://1fichier.com/dir/FOLDER123"),
            Some(Kind::Folder("FOLDER123".into()))
        );
        assert_eq!(kind("https://1fichier.com/"), None);
        assert_eq!(kind("https://www.1fichier.com/"), None);
        assert_eq!(kind("https://1fichier.com/console/index.pl"), None);
    }

    #[test]
    fn reads_the_page() {
        let html = r#"<table><tr><td>Filename :</td><td class="normal">Mi &amp; archivo.zip</td></tr>
            <tr><td>Size :</td><td class="normal">1.5 GB</td></tr></table>
            <form method="post"><input type="hidden" name="adz" value="AB12">
            <input type="submit" value="Free Download"></form>"#;
        assert_eq!(
            parse_page(html),
            Page {
                name: Some("Mi & archivo.zip".into()),
                size: Some(1_500_000_000),
                adz: Some("AB12".into())
            }
        );
        assert_eq!(parse_size("350 MB"), Some(350_000_000));
        assert_eq!(parse_size("raro"), None);
    }

    #[test]
    fn reads_waits_and_links() {
        assert_eq!(
            wait_minutes("<div>Warning ! You must wait 14 minutes before downloading</div>"),
            Some(14)
        );
        assert_eq!(wait_minutes("nada"), None);
        let ok = r#"<a href="https://www.1fichier.com/">inicio</a>
            <a href="https://a-5.1fichier.com/c123456789" class="ok btn-general btn-orange">Click here</a>"#;
        assert_eq!(
            download_link(ok).unwrap(),
            "https://a-5.1fichier.com/c123456789"
        );
        assert!(download_link("<a href=\"https://otro.com/x\">").is_none());
    }
}
