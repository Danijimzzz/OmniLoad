//! Rootz (rootz.so).
//!
//! La web es una app de Next.js. Lo que hace el navegador, y por tanto este
//! plugin:
//!
//! 1. Abre `/d/ID`: da unas cookies de DDoS-Guard y un `pageToken` dentro del
//!    HTML.
//! 2. `GET /api/files/download-by-short?shortId=ID` con la cabecera
//!    `X-Page-Token`: nombre, tamaño y estado del archivo.
//! 3. `GET /api/files/proxy-download/ID`: responde con una redirección a un
//!    enlace firmado de almacenamiento (válido 24 horas, acepta `Range`).
//!
//! La gratuita está limitada: solo deja pedir el paso 3 una vez cada dos
//! minutos por conexión y su API anuncia una velocidad gratis muy baja
//! (`freeSpeedKbps`). Cada arranque de una descarga pide el paso 3 una sola
//! vez; si Rootz contesta 429, el error dice cuánto esperar.
//!
//! Si Rootz cambia su web, lo único frágil es [`page_token`] y las rutas de
//! `api()`.

use omni_core::{Error, RemoteInfo};
use omni_grab::{CheckFuture, Found};
use omni_queue::{ResolveFuture, ResolvedLink};
use reqwest::header::{COOKIE, HeaderMap, REFERER, SET_COOKIE};
use url::Url;

use crate::{Host, json_size, on_domain, segments, status_error};

const NAME: &str = "Rootz";
const BASE: &str = "https://www.rootz.so";

/// El código del archivo de un enlace `https://www.rootz.so/d/CODIGO`.
pub(crate) fn parse(url: &Url) -> Option<String> {
    if !on_domain(url, "rootz.so") {
        return None;
    }
    match segments(url).iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["d", code] if !code.is_empty() => Some(code.to_owned()),
        _ => None,
    }
}

/// El `pageToken` que la página lleva dentro de sus datos de Next.js.
pub(crate) fn page_token(html: &str) -> Option<String> {
    // En el HTML las comillas de esos datos van escapadas (`\"`).
    let text = html.replace("\\\"", "\"");
    let start = text.find("\"pageToken\":\"")? + "\"pageToken\":\"".len();
    let end = text[start..].find('"')? + start;
    let token = &text[start..end];
    (!token.is_empty()).then(|| token.to_owned())
}

/// Las cookies que pone una respuesta, listas para devolverlas en `Cookie`.
fn cookies(headers: &HeaderMap) -> String {
    headers
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|c| c.split(';').next())
        .map(str::trim)
        .filter(|c| c.contains('='))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Rootz.
pub struct Rootz {
    base: String,
}

impl Rootz {
    pub fn new() -> Self {
        Self::with_base(BASE)
    }

    /// Con otra dirección (pruebas).
    pub fn with_base(base: impl Into<String>) -> Self {
        Self { base: base.into() }
    }

    fn api(&self, path: &str) -> omni_core::Result<Url> {
        Url::parse(&format!("{}/{path}", self.base.trim_end_matches('/')))
            .map_err(|e| Error::InvalidUrl(e.to_string()))
    }

    /// Abre la página del archivo: cookies y `pageToken`.
    async fn open_page(
        &self,
        client: &reqwest::Client,
        code: &str,
    ) -> omni_core::Result<(String, Option<String>)> {
        let resp = client.get(self.api(&format!("d/{code}"))?).send().await?;
        let status = resp.status();
        let cookie = cookies(resp.headers());
        if !status.is_success() {
            return Err(status_error(status));
        }
        let html = resp.text().await?;
        Ok((cookie, page_token(&html)))
    }
}

impl Default for Rootz {
    fn default() -> Self {
        Self::new()
    }
}

impl Host for Rootz {
    fn name(&self) -> &str {
        NAME
    }

    fn handles(&self, url: &Url) -> bool {
        parse(url).is_some()
    }

    fn check<'a>(&'a self, client: &'a reqwest::Client, url: &'a Url) -> CheckFuture<'a> {
        Box::pin(async move {
            let code = parse(url).ok_or_else(|| Error::InvalidUrl(url.to_string()))?;
            let (cookie, token) = self.open_page(client, &code).await?;
            let mut req = client
                .get(self.api(&format!("api/files/download-by-short?shortId={code}"))?)
                .header(REFERER, self.api(&format!("d/{code}"))?.as_str());
            if let Some(t) = &token {
                req = req.header("X-Page-Token", t);
            }
            if !cookie.is_empty() {
                req = req.header(COOKIE, &cookie);
            }
            let resp = req.send().await?;
            let status = resp.status();
            let json: serde_json::Value = resp.json().await.unwrap_or_default();
            if !status.is_success() || json["success"] != true {
                return Err(match json["error"].as_str().or(json["message"].as_str()) {
                    Some(m) if status.as_u16() != 404 => Error::host(NAME, m),
                    _ => status_error(status),
                });
            }
            let data = &json["data"];
            match data["status"].as_str() {
                Some("active") | None => {}
                Some(_) => return Err(Error::LinkExpired { status: 404 }),
            }
            if data["passwordProtected"] == true {
                return Err(Error::host(NAME, "el archivo tiene contraseña"));
            }
            if data["downloadAllowed"] == false {
                return Err(Error::host(NAME, "la descarga no está permitida"));
            }
            Ok(Found::File(RemoteInfo {
                url: url.clone(),
                size: json_size(&data["size"]),
                accepts_ranges: true,
                etag: None,
                last_modified: None,
                filename: omni_core::filename::sanitize(data["fileName"].as_str().unwrap_or(&code)),
            }))
        })
    }

    fn resolve<'a>(&'a self, client: &'a reqwest::Client, url: &'a Url) -> ResolveFuture<'a> {
        Box::pin(async move {
            let code = parse(url).ok_or_else(|| Error::InvalidUrl(url.to_string()))?;
            let (cookie, _) = self.open_page(client, &code).await?;
            // Se pide 1 byte: la respuesta es una redirección al enlace firmado
            // y, al seguirla, `resp.url()` es ese enlace. El cuerpo no se lee.
            let mut req = client
                .get(self.api(&format!("api/files/proxy-download/{code}"))?)
                .header(REFERER, self.api(&format!("d/{code}"))?.as_str())
                .header(reqwest::header::RANGE, "bytes=0-0");
            if !cookie.is_empty() {
                req = req.header(COOKIE, &cookie);
            }
            let resp = req.send().await?;
            let status = resp.status();
            if status.as_u16() == 429 {
                let wait = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .map_or(120, |s| s.max(1));
                return Err(Error::host(
                    NAME,
                    format!(
                        "Rootz solo deja pedir una descarga cada pocos minutos; espera {wait} s"
                    ),
                ));
            }
            // Sin redirección seguida, la Location está en la propia respuesta.
            let final_url = if status.is_redirection() {
                resp.headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|l| resp.url().join(l).ok())
            } else if status.is_success() {
                Some(resp.url().clone())
            } else {
                return Err(status_error(status));
            };
            match final_url {
                Some(u) if !on_domain(&u, "rootz.so") => Ok(ResolvedLink {
                    url: u.into(),
                    ..ResolvedLink::default()
                }),
                _ => Err(Error::host(
                    NAME,
                    "Rootz no ha dado el enlace de descarga (puede haber cambiado su web)",
                )),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_links() {
        let p = |s: &str| parse(&Url::parse(s).unwrap());
        assert_eq!(p("https://www.rootz.so/d/oYeLw"), Some("oYeLw".into()));
        assert_eq!(p("https://rootz.so/d/abc?x=1"), Some("abc".into()));
        assert_eq!(p("https://www.rootz.so/"), None);
        assert_eq!(p("https://www.rootz.so/dashboard"), None);
        assert_eq!(p("https://notrootz.so/d/abc"), None);
    }

    #[test]
    fn finds_page_token() {
        let html = r#"<script>self.__next_f.push([1,"5:[\"$\",\"$L1a\",null,{\"shortId\":\"oYeLw\",\"pageToken\":\"abc.DEF-_1\"}]"])</script>"#;
        assert_eq!(page_token(html), Some("abc.DEF-_1".into()));
        assert_eq!(page_token("<html></html>"), None);
    }

    #[test]
    fn cookies_keep_only_name_and_value() {
        let mut h = HeaderMap::new();
        h.append(
            SET_COOKIE,
            "__ddg8_=abc; Domain=.rootz.so; Path=/".parse().unwrap(),
        );
        h.append(SET_COOKIE, "__ddg9_=1.2.3.4; Path=/".parse().unwrap());
        assert_eq!(cookies(&h), "__ddg8_=abc; __ddg9_=1.2.3.4");
    }
}
