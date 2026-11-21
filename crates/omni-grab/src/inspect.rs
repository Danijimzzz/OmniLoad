//! Comprobar un enlace genérico (HTTP): ¿es un archivo o una página?

use reqwest::StatusCode;
use reqwest::header::{CONTENT_TYPE, RANGE};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::links::{extract_links, page_title};
use omni_core::RemoteInfo;

/// Cuánto HTML leemos como mucho de una página.
const MAX_PAGE_BYTES: usize = 4 * 1024 * 1024;
/// Enlaces como mucho por página.
pub const MAX_LINKS_PER_PAGE: usize = 500;

/// Qué hay detrás de un enlace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Found {
    /// Un archivo descargable.
    File(RemoteInfo),
    /// Una página web, con su título y los enlaces a archivos que contiene.
    /// Cada enlace se comprueba después por separado.
    Page {
        url: Url,
        title: Option<String>,
        links: Vec<Url>,
    },
    /// Una carpeta de un host (Mega, Gofile, Google Drive…) cuyo plugin ya
    /// sabe qué archivos tiene, con su nombre y tamaño: no hace falta
    /// comprobarlos uno a uno.
    Folder {
        name: Option<String>,
        files: Vec<FolderFile>,
    },
}

/// Un archivo de una carpeta de un host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderFile {
    /// Enlace al archivo (el que guardará la cola y resolverá el plugin al
    /// descargar).
    pub url: Url,
    pub name: String,
    pub size: Option<u64>,
    /// ¿Se puede descargar por bloques y reanudar?
    pub resumable: bool,
}

/// Mira qué hay detrás de `url`: un archivo o una página con enlaces.
///
/// Con una sola petición en el caso normal: un `GET` con `Range: bytes=0-0`
/// dice a la vez el tipo de contenido y, si es un archivo, su tamaño, su
/// nombre y si acepta rangos. Así comprobar cien enlaces cuesta cien
/// peticiones, no doscientas.
pub async fn inspect(client: &reqwest::Client, url: &Url) -> omni_core::Result<Found> {
    let resp = client
        .get(url.clone())
        .header(RANGE, "bytes=0-0")
        .send()
        .await?;
    let status = resp.status();
    let is_html = (status == StatusCode::OK || status == StatusCode::PARTIAL_CONTENT)
        && resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(is_html_type);
    if !is_html {
        let final_url = resp.url().clone();
        let headers = resp.headers().clone();
        // Sin leer el cuerpo: si el servidor ignoró el rango y manda el
        // archivo entero, soltar la respuesta corta la conexión.
        drop(resp);
        return omni_core::info_from_response(status, final_url, &headers).map(Found::File);
    }

    // Una página. Si el servidor respetó el rango solo tenemos un byte:
    // hay que pedirla entera.
    let resp = if status == StatusCode::PARTIAL_CONTENT {
        drop(resp);
        client.get(url.clone()).send().await?.error_for_status()?
    } else {
        resp
    };
    let page_url = resp.url().clone();
    let html = read_limited(resp, MAX_PAGE_BYTES).await?;
    let mut links = extract_links(&html, &page_url);
    links.truncate(MAX_LINKS_PER_PAGE);
    Ok(Found::Page {
        url: page_url,
        title: page_title(&html),
        links,
    })
}

fn is_html_type(ct: &str) -> bool {
    let ct = ct.split(';').next().unwrap_or("").trim();
    ct.eq_ignore_ascii_case("text/html") || ct.eq_ignore_ascii_case("application/xhtml+xml")
}

async fn read_limited(mut resp: reqwest::Response, max: usize) -> omni_core::Result<String> {
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        let room = max - buf.len();
        buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if buf.len() >= max {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}
