//! Sondeo: una petición `GET` con `Range: bytes=0-0` averigua tamaño, nombre,
//! validadores (ETag, Last-Modified) y si el servidor acepta rangos.
//!
//! Usamos GET con rango en lugar de HEAD porque muchos servidores responden
//! mal a HEAD o no anuncian `Accept-Ranges` aunque sí acepten rangos; un 206
//! real es la única prueba fiable.

use reqwest::StatusCode;
use reqwest::header::{
    CONTENT_DISPOSITION, CONTENT_ENCODING, CONTENT_RANGE, ETAG, HeaderMap, LAST_MODIFIED, RANGE,
};
use url::Url;

use crate::{Error, Result, filename};

/// Lo que sabemos del archivo remoto.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteInfo {
    /// URL tras seguir redirecciones; las peticiones de bloques van aquí.
    pub url: Url,
    /// Tamaño total, si se conoce.
    pub size: Option<u64>,
    /// ¿Acepta peticiones con rango?
    pub accepts_ranges: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// Nombre propuesto, ya saneado.
    pub filename: String,
}

impl RemoteInfo {
    /// ¿Se puede descargar por bloques y reanudar?
    pub fn is_segmentable(&self) -> bool {
        self.accepts_ranges && self.size.is_some_and(|s| s > 0)
    }

    /// Validador para `If-Range`: el ETag si es fuerte, si no Last-Modified.
    pub fn if_range(&self) -> Option<&str> {
        match &self.etag {
            Some(e) if !e.starts_with("W/") => Some(e),
            _ => self.last_modified.as_deref(),
        }
    }
}

/// Sondea `url` con el cliente dado.
pub async fn probe(client: &reqwest::Client, url: &Url) -> Result<RemoteInfo> {
    probe_with(client, url, &HeaderMap::new()).await
}

/// Como [`probe`], con cabeceras extra (cookies o tokens de un host).
pub async fn probe_with(
    client: &reqwest::Client,
    url: &Url,
    extra: &HeaderMap,
) -> Result<RemoteInfo> {
    let resp = client
        .get(url.clone())
        .headers(extra.clone())
        .header(RANGE, "bytes=0-0")
        .send()
        .await?;
    let status = resp.status();
    let final_url = resp.url().clone();
    let headers = resp.headers().clone();
    // No leemos el cuerpo: si el servidor ignora el rango y manda el archivo
    // entero, soltar la respuesta cierra la conexión.
    drop(resp);

    let (size, accepts_ranges) = match status {
        StatusCode::PARTIAL_CONTENT => {
            let total = headers
                .get(CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_content_range)
                .map(|cr| cr.total);
            match total {
                Some(Some(t)) => (Some(t), true),
                // "bytes 0-0/*": acepta rangos pero no sabemos el tamaño.
                _ => (None, false),
            }
        }
        StatusCode::OK => (content_length(&headers), false),
        StatusCode::RANGE_NOT_SATISFIABLE => {
            // Archivo vacío: "bytes */0".
            let total = headers
                .get(CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("bytes */"))
                .and_then(|v| v.trim().parse::<u64>().ok());
            match total {
                Some(0) => (Some(0), false),
                _ => return Err(Error::HttpStatus { status: 416 }),
            }
        }
        s => return Err(status_error(s)),
    };

    let header_str = |name| {
        headers
            .get(name)
            .and_then(|v: &reqwest::header::HeaderValue| v.to_str().ok())
            .map(str::to_string)
    };
    let filename = filename::choose(header_str(CONTENT_DISPOSITION).as_deref(), &final_url);

    Ok(RemoteInfo {
        url: final_url,
        size,
        accepts_ranges,
        etag: header_str(ETAG),
        last_modified: header_str(LAST_MODIFIED),
        filename,
    })
}

/// Convierte un código HTTP inesperado en el error adecuado.
pub(crate) fn status_error(status: StatusCode) -> Error {
    match status.as_u16() {
        s @ (403 | 404 | 410) => Error::LinkExpired { status: s },
        s => Error::HttpStatus { status: s },
    }
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    // Con compresión, Content-Length no es el tamaño del archivo.
    if headers.contains_key(CONTENT_ENCODING) {
        return None;
    }
    headers
        .get(reqwest::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

/// `Content-Range: bytes start-end/total`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ContentRange {
    pub start: u64,
    pub end: u64,
    pub total: Option<u64>,
}

pub(crate) fn parse_content_range(v: &str) -> Option<ContentRange> {
    let rest = v.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    Some(ContentRange {
        start: start.trim().parse().ok()?,
        end: end.trim().parse().ok()?,
        total: match total.trim() {
            "*" => None,
            t => Some(t.parse().ok()?),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_range() {
        assert_eq!(
            parse_content_range("bytes 0-0/1234"),
            Some(ContentRange {
                start: 0,
                end: 0,
                total: Some(1234)
            })
        );
        assert_eq!(parse_content_range("bytes 5-9/*").unwrap().total, None);
        assert_eq!(parse_content_range("items 0-1/2"), None);
    }
}
