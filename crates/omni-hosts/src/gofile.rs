//! Gofile.
//!
//! La API actual pide un **token de invitado** (`POST /accounts`) y una
//! cabecera `X-Website-Token` que la web calcula en JavaScript. Es la parte
//! más frágil: Gofile ha cambiado la fórmula varias veces. Todo lo que
//! depende de ella está en [`website_token`] y en las constantes de abajo;
//! si algún día deja de ir, solo hay que tocar eso.
//!
//! Los enlaces directos de los archivos solo valen con la cookie
//! `accountToken` del mismo invitado, así que la resolución las devuelve.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use omni_core::Error;
use omni_grab::{CheckFuture, FolderFile, Found};
use omni_queue::{ResolveFuture, ResolvedLink};
use reqwest::header::{AUTHORIZATION, COOKIE, HeaderMap, HeaderValue, USER_AGENT};
use sha2::{Digest, Sha256};
use url::Url;

use crate::{Host, json_size, on_domain, segments};

const NAME: &str = "Gofile";
/// Lo que la web de Gofile manda como navegador e idioma, y la sal de su
/// fórmula. Si Gofile los cambia, hay que actualizarlos aquí.
const WEB_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const WEB_LANGUAGE: &str = "en-US";
const WEB_SECRET: &str = "5d4f7g8sd45fsd";
/// El token de la web cambia cada 4 horas.
const SLOT_SECONDS: u64 = 14_400;
const MAX_DEPTH: usize = 3;

/// El `X-Website-Token` de ese momento para ese token de invitado.
pub(crate) fn website_token(account_token: &str, unix_seconds: u64) -> String {
    let slot = unix_seconds / SLOT_SECONDS;
    let text = format!("{WEB_USER_AGENT}::{WEB_LANGUAGE}::{account_token}::{slot}::{WEB_SECRET}");
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// El código de la carpeta y, si el enlace trae `?file=ID`, el archivo.
pub(crate) fn parse(url: &Url) -> Option<(String, Option<String>)> {
    if !on_domain(url, "gofile.io") {
        return None;
    }
    let seg = segments(url);
    match seg.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["d", code, ..] => Some((
            code.to_owned(),
            url.query_pairs()
                .find(|(k, _)| k == "file")
                .map(|(_, v)| v.into_owned()),
        )),
        _ => None,
    }
}

/// Gofile.
pub struct Gofile {
    api: String,
    token: Mutex<Option<String>>,
}

struct Child {
    id: String,
    name: String,
    size: Option<u64>,
    link: Option<String>,
    is_folder: bool,
}

impl Gofile {
    pub fn new() -> Self {
        Self::with_api("https://api.gofile.io")
    }

    /// Con otra dirección para la API (pruebas).
    pub fn with_api(api: impl Into<String>) -> Self {
        Self {
            api: api.into(),
            token: Mutex::new(None),
        }
    }

    fn cached_token(&self) -> Option<String> {
        self.token.lock().expect("token envenenado").clone()
    }

    /// El token de invitado: el que ya se tiene o uno nuevo.
    async fn token(&self, client: &reqwest::Client) -> omni_core::Result<String> {
        if let Some(t) = self.cached_token() {
            return Ok(t);
        }
        let resp = client
            .post(format!("{}/accounts", self.api.trim_end_matches('/')))
            .header(USER_AGENT, WEB_USER_AGENT)
            .send()
            .await?;
        let status = resp.status();
        let json: serde_json::Value = resp.json().await.unwrap_or_default();
        match json["data"]["token"].as_str() {
            Some(t) if json["status"] == "ok" => {
                *self.token.lock().expect("token envenenado") = Some(t.to_owned());
                Ok(t.to_owned())
            }
            _ => Err(if status.is_success() {
                Error::host(NAME, "no se pudo crear la cuenta de invitado")
            } else {
                crate::status_error(status)
            }),
        }
    }

    fn headers(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(USER_AGENT, HeaderValue::from_static(WEB_USER_AGENT));
        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {token}")) {
            h.insert(AUTHORIZATION, v);
        }
        if let Ok(v) = HeaderValue::from_str(&format!("accountToken={token}")) {
            h.insert(COOKIE, v);
        }
        h
    }

    /// Los hijos de una carpeta (y su nombre). Si el token caducó, pide uno
    /// nuevo una vez.
    async fn contents(
        &self,
        client: &reqwest::Client,
        id: &str,
    ) -> omni_core::Result<(Option<String>, Vec<Child>, String)> {
        for attempt in 0..2 {
            let token = self.token(client).await?;
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or_default();
            let url = format!(
                "{}/contents/{id}?contentFilter=&page=1&pageSize=1000&sortField=name&sortDirection=1",
                self.api.trim_end_matches('/')
            );
            let resp = client
                .get(url)
                .headers(Self::headers(&token))
                .header("X-Website-Token", website_token(&token, now))
                .header("X-BL", WEB_LANGUAGE)
                .send()
                .await?;
            let http = resp.status();
            let json: serde_json::Value = resp.json().await.unwrap_or_default();
            let status = json["status"].as_str().unwrap_or("");
            if status == "ok" {
                let data = &json["data"];
                let children = data["children"]
                    .as_object()
                    .map(|o| {
                        o.values()
                            .filter_map(|c| {
                                Some(Child {
                                    id: c["id"].as_str()?.to_owned(),
                                    name: c["name"].as_str()?.to_owned(),
                                    size: json_size(&c["size"]),
                                    link: c["link"].as_str().map(str::to_owned),
                                    is_folder: c["type"] == "folder",
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                return Ok((data["name"].as_str().map(str::to_owned), children, token));
            }
            match status {
                "error-notFound" => return Err(Error::LinkExpired { status: 404 }),
                "error-passwordRequired" | "error-passwordWrong" => {
                    return Err(Error::host(NAME, "la carpeta tiene contraseña"));
                }
                "error-notPublic" => return Err(Error::host(NAME, "la carpeta no es pública")),
                "error-rateLimit" => {
                    return Err(Error::host(
                        NAME,
                        "demasiadas peticiones seguidas; prueba en unos minutos",
                    ));
                }
                // Token caducado o rechazado: uno nuevo y otra vez.
                "error-token" | "error-auth" | "error-notAuthorized" if attempt == 0 => {
                    *self.token.lock().expect("token envenenado") = None;
                }
                other => {
                    return Err(if other.is_empty() {
                        crate::status_error(http)
                    } else {
                        Error::host(
                            NAME,
                            format!(
                                "respuesta inesperada ({other}); puede que Gofile haya cambiado su API"
                            ),
                        )
                    });
                }
            }
        }
        Err(Error::host(NAME, "Gofile rechazó la cuenta de invitado"))
    }

    async fn walk(
        &self,
        client: &reqwest::Client,
        id: &str,
        depth: usize,
        out: &mut Vec<(String, FolderFile)>,
    ) -> omni_core::Result<Option<String>> {
        let (name, children, _) = self.contents(client, id).await?;
        for c in children {
            if c.is_folder {
                if depth < MAX_DEPTH {
                    Box::pin(self.walk(client, &c.id, depth + 1, out)).await?;
                }
            } else {
                let mut u = Url::parse(&format!("https://gofile.io/d/{id}"))
                    .map_err(|e| Error::InvalidUrl(e.to_string()))?;
                u.query_pairs_mut().append_pair("file", &c.id);
                out.push((
                    c.id,
                    FolderFile {
                        url: u,
                        name: c.name,
                        size: c.size,
                        resumable: true,
                    },
                ));
            }
        }
        Ok(name)
    }
}

impl Default for Gofile {
    fn default() -> Self {
        Self::new()
    }
}

impl Host for Gofile {
    fn name(&self) -> &str {
        NAME
    }

    fn handles(&self, url: &Url) -> bool {
        parse(url).is_some()
    }

    fn parallel(&self) -> usize {
        2
    }

    fn check<'a>(&'a self, client: &'a reqwest::Client, url: &'a Url) -> CheckFuture<'a> {
        Box::pin(async move {
            let (code, file) = parse(url).ok_or_else(|| Error::InvalidUrl(url.to_string()))?;
            let mut all = Vec::new();
            let name = self.walk(client, &code, 1, &mut all).await?;
            let files: Vec<FolderFile> = all
                .into_iter()
                .filter(|(id, _)| file.as_ref().is_none_or(|f| f == id))
                .map(|(_, f)| f)
                .collect();
            if files.is_empty() {
                return Err(Error::host(NAME, "la carpeta no tiene archivos"));
            }
            Ok(Found::Folder { name, files })
        })
    }

    fn resolve<'a>(&'a self, client: &'a reqwest::Client, url: &'a Url) -> ResolveFuture<'a> {
        Box::pin(async move {
            let (code, file) = parse(url).ok_or_else(|| Error::InvalidUrl(url.to_string()))?;
            let (_, children, token) = self.contents(client, &code).await?;
            let files: Vec<&Child> = children.iter().filter(|c| !c.is_folder).collect();
            let chosen = match &file {
                Some(id) => files.iter().find(|c| &c.id == id),
                None if files.len() == 1 => files.first(),
                None => None,
            };
            let Some(chosen) = chosen else {
                return Err(Error::host(NAME, "el archivo ya no está en la carpeta"));
            };
            let link = chosen
                .link
                .clone()
                .ok_or_else(|| Error::host(NAME, "Gofile no dio el enlace de descarga"))?;
            Ok(ResolvedLink {
                url: link,
                headers: Self::headers(&token),
                filename: Some(chosen.name.clone()),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_links() {
        let p = |s: &str| parse(&Url::parse(s).unwrap());
        assert_eq!(
            p("https://gofile.io/d/AbC12x"),
            Some(("AbC12x".into(), None))
        );
        assert_eq!(
            p("https://gofile.io/d/AbC12x?file=f1"),
            Some(("AbC12x".into(), Some("f1".into())))
        );
        assert_eq!(p("https://gofile.io/"), None);
        assert_eq!(p("https://gofile.io/contents/x"), None);
    }

    #[test]
    fn website_token_is_stable_within_a_slot() {
        let a = website_token("tok", 14_400 * 10 + 5);
        assert_eq!(a, website_token("tok", 14_400 * 10 + 14_000));
        assert_ne!(a, website_token("tok", 14_400 * 11));
        assert_ne!(a, website_token("otro", 14_400 * 10));
        assert_eq!(a.len(), 64);
    }
}
