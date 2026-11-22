//! Mega.
//!
//! Todo lo que hay en Mega va cifrado y la clave viaja en el `#fragmento` del
//! enlace, así que no hace falta cuenta: se pregunta a la API
//! (`g.api.mega.co.nz`) por el tamaño, el nombre (cifrado) y una dirección
//! temporal de descarga, y el contenido se descifra al bajarlo.
//!
//! - **Archivo:** clave de 256 bits = clave AES-128 (las dos mitades con XOR),
//!   nonce de 64 bits y MAC. El contenido es AES-128-CTR; el nombre,
//!   AES-128-CBC.
//! - **Carpeta:** `f` lista los nodos con sus claves cifradas con la de la
//!   carpeta (AES-128-ECB).
//!
//! La descarga la hace este módulo, no el motor HTTP: baja con una conexión
//! y `Range`, descifra por el camino y puede seguir donde se quedó porque CTR
//! se puede empezar en cualquier bloque de 16 bytes. **No se comprueba el MAC
//! de Mega** (la integridad la da TLS); queda para más adelante.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use aes::Aes128;
use aes::cipher::generic_array::GenericArray;
use aes::cipher::{
    BlockDecrypt, BlockDecryptMut, KeyInit, KeyIvInit, StreamCipher, StreamCipherSeek,
};
use base64::Engine;
use omni_core::{DownloadOutcome, Error, Progress, RemoteInfo, part_path_for};
use omni_grab::{CheckFuture, FolderFile, Found};
use omni_queue::{CustomDownload, ResolveFuture, ResolvedLink, RunFuture};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{Host, on_domain, segments};

const NAME: &str = "Mega";
type Ctr = ctr::Ctr128BE<Aes128>;
type CbcDec = cbc::Decryptor<Aes128>;
/// Veces que se repite una petición a la API si Mega dice «inténtalo luego».
const API_TRIES: usize = 5;
/// Cortes de red seguidos que se aguantan antes de rendirse.
const STREAM_TRIES: usize = 6;

// ------------------------------------------------------------------ enlaces

/// La clave de un archivo: AES-128 y nonce de CTR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileKey {
    pub aes: [u8; 16],
    pub nonce: [u8; 8],
}

impl FileKey {
    /// De la clave de 32 bytes del enlace.
    fn from_bytes(k: &[u8]) -> Option<Self> {
        if k.len() != 32 {
            return None;
        }
        let mut aes = [0u8; 16];
        for (i, b) in aes.iter_mut().enumerate() {
            *b = k[i] ^ k[16 + i];
        }
        Some(Self {
            aes,
            nonce: k[16..24].try_into().ok()?,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Link {
    File {
        id: String,
        key: Vec<u8>,
    },
    /// Carpeta, y el nodo si el enlace apunta a un archivo suyo.
    Folder {
        id: String,
        key: Vec<u8>,
        node: Option<String>,
    },
}

pub(crate) fn b64(s: &str) -> Option<Vec<u8>> {
    let clean: String = s
        .trim()
        .trim_end_matches('=')
        .chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            ',' => '-',
            c => c,
        })
        .collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(clean)
        .ok()
        .filter(|b| !b.is_empty())
}

fn b64_encode(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

pub(crate) fn parse(url: &Url) -> Option<Link> {
    if !(on_domain(url, "mega.nz") || on_domain(url, "mega.co.nz") || on_domain(url, "mega.io")) {
        return None;
    }
    let frag = url.fragment().unwrap_or("");
    let seg = segments(url);
    let seg: Vec<&str> = seg.iter().map(String::as_str).collect();
    match seg[..] {
        ["file", id] => Some(Link::File {
            id: id.to_owned(),
            key: b64(frag.split('/').next()?)?,
        }),
        ["folder", id] => {
            let mut parts = frag.split('/');
            let key = b64(parts.next()?)?;
            let node = match (parts.next(), parts.next()) {
                (Some("file" | "folder"), Some(n)) if !n.is_empty() => Some(n.to_owned()),
                _ => None,
            };
            Some(Link::Folder {
                id: id.to_owned(),
                key,
                node,
            })
        }
        [] => {
            // Formato antiguo: `#!ID!CLAVE` y `#F!ID!CLAVE[!NODO]`.
            let mut p = frag.split('!');
            match p.next()? {
                "" => Some(Link::File {
                    id: p.next()?.to_owned(),
                    key: b64(p.next()?)?,
                }),
                "F" => Some(Link::Folder {
                    id: p.next()?.to_owned(),
                    key: b64(p.next()?)?,
                    node: p.next().filter(|n| !n.is_empty()).map(str::to_owned),
                }),
                _ => None,
            }
        }
        _ => None,
    }
}

/// El enlace de un archivo suelto dentro de una carpeta.
fn folder_file_url(id: &str, key: &[u8], node: &str) -> Option<Url> {
    Url::parse(&format!(
        "https://mega.nz/folder/{id}#{}/file/{node}",
        b64_encode(key)
    ))
    .ok()
}

// ------------------------------------------------------------------- cifrado

fn ecb_decrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut out = data.to_vec();
    for block in out.as_chunks_mut::<16>().0 {
        cipher.decrypt_block(GenericArray::from_mut_slice(&mut block[..]));
    }
    out
}

/// El nombre guardado en los atributos de un nodo (`MEGA{"n":"nombre"}`).
pub(crate) fn decrypt_name(key: &[u8; 16], attr_b64: &str) -> Option<String> {
    let mut data = b64(attr_b64)?;
    if data.is_empty() || data.len() % 16 != 0 {
        return None;
    }
    let dec = CbcDec::new(key.into(), &[0u8; 16].into());
    dec.decrypt_padded_mut::<aes::cipher::block_padding::NoPadding>(&mut data)
        .ok()?;
    let end = data.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1);
    let text = std::str::from_utf8(&data[..end])
        .ok()?
        .strip_prefix("MEGA")?;
    let json: Value = serde_json::from_str(text).ok()?;
    json["n"].as_str().map(str::to_owned)
}

fn ctr_at(key: &FileKey, offset: u64) -> Ctr {
    let mut iv = [0u8; 16];
    iv[..8].copy_from_slice(&key.nonce);
    let mut c = Ctr::new((&key.aes).into(), (&iv).into());
    c.seek(offset);
    c
}

// ----------------------------------------------------------------------- API

fn api_error(code: i64) -> Error {
    match code {
        -9 | -8 => Error::LinkExpired { status: 404 },
        -16 | -6 => Error::host(NAME, "enlace bloqueado o retirado por Mega"),
        -17 => Error::host(
            NAME,
            "cuota de transferencia agotada: espera unas horas o cambia de red",
        ),
        -4 => Error::host(NAME, "Mega pide ir más despacio: prueba en unos minutos"),
        -2 | -14 => Error::host(NAME, "enlace o clave no válidos"),
        -11 => Error::host(NAME, "sin acceso a este enlace"),
        c => Error::host(NAME, format!("error de Mega ({c})")),
    }
}

/// Un archivo listo para bajar.
struct FileInfo {
    size: u64,
    name: String,
    key: FileKey,
    /// Dirección temporal de descarga.
    url: String,
}

/// Un nodo de una carpeta.
struct Node {
    id: String,
    is_folder: bool,
    size: Option<u64>,
    name: String,
    /// Clave ya descifrada (16 bytes en carpetas, 32 en archivos).
    key: Vec<u8>,
}

/// Mega.
pub struct Mega {
    api: String,
    seq: AtomicU32,
}

impl Mega {
    pub fn new() -> Self {
        Self::with_api("https://g.api.mega.co.nz")
    }

    /// Con otra dirección para la API (pruebas).
    pub fn with_api(api: impl Into<String>) -> Self {
        Self {
            api: api.into(),
            seq: AtomicU32::new(1_000_000 + (std::process::id() % 1000)),
        }
    }

    async fn call(
        &self,
        client: &reqwest::Client,
        folder: Option<&str>,
        cmd: Value,
    ) -> omni_core::Result<Value> {
        api_call(client, &self.api, &self.seq, folder, cmd).await
    }

    async fn nodes(
        &self,
        client: &reqwest::Client,
        id: &str,
        key: &[u8; 16],
    ) -> omni_core::Result<(Option<String>, Vec<Node>)> {
        let resp = self
            .call(client, Some(id), json!({"a": "f", "c": 1, "r": 1}))
            .await?;
        let mut root_name = None;
        let mut out = Vec::new();
        for n in resp["f"].as_array().map(Vec::as_slice).unwrap_or_default() {
            let (Some(h), Some(t)) = (n["h"].as_str(), n["t"].as_i64()) else {
                continue;
            };
            let attr = n["a"].as_str().unwrap_or("");
            if t == 2 || h == id {
                root_name = decrypt_name(key, attr);
                continue;
            }
            let Some(enc) = n["k"].as_str().and_then(|k| {
                k.split('/')
                    .find_map(|e| e.split_once(':').map(|(_, v)| v.to_owned()))
            }) else {
                continue;
            };
            let Some(raw) = b64(&enc) else { continue };
            let node_key = ecb_decrypt(key, &raw);
            let aes_key: Option<[u8; 16]> = match t {
                0 => FileKey::from_bytes(&node_key).map(|k| k.aes),
                _ => node_key.get(..16).and_then(|k| k.try_into().ok()),
            };
            let Some(aes_key) = aes_key else { continue };
            out.push(Node {
                id: h.to_owned(),
                is_folder: t == 1,
                size: n["s"].as_u64(),
                name: decrypt_name(&aes_key, attr).unwrap_or_else(|| h.to_owned()),
                key: node_key,
            });
        }
        Ok((root_name, out))
    }

    /// Todo lo que hace falta para bajar el archivo de un enlace.
    async fn file_info(
        &self,
        client: &reqwest::Client,
        link: &Link,
    ) -> omni_core::Result<FileInfo> {
        file_info(client, &self.api, &self.seq, link).await
    }
}

async fn api_call(
    client: &reqwest::Client,
    api: &str,
    seq: &AtomicU32,
    folder: Option<&str>,
    cmd: Value,
) -> omni_core::Result<Value> {
    let mut delay = Duration::from_millis(400);
    for attempt in 0..API_TRIES {
        let mut url = Url::parse(&format!("{}/cs", api.trim_end_matches('/')))
            .map_err(|e| Error::InvalidUrl(e.to_string()))?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("id", &seq.fetch_add(1, Ordering::Relaxed).to_string());
            if let Some(f) = folder {
                q.append_pair("n", f);
            }
        }
        let resp = client.post(url).json(&json!([cmd])).send().await?;
        if !resp.status().is_success() {
            return Err(crate::status_error(resp.status()));
        }
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        // La respuesta es una lista con un elemento: el resultado o un
        // número negativo con el error.
        let first = body
            .as_array()
            .and_then(|a| a.first().cloned())
            .or_else(|| body.as_i64().map(Value::from))
            .unwrap_or(Value::Null);
        match first.as_i64() {
            Some(-3) if attempt + 1 < API_TRIES => {
                tokio::time::sleep(delay).await;
                delay *= 2;
            }
            Some(code) if code < 0 => return Err(api_error(code)),
            _ if first.is_object() => return Ok(first),
            _ => return Err(Error::host(NAME, "respuesta de Mega ilegible")),
        }
    }
    Err(Error::host(NAME, "Mega no contesta: prueba más tarde"))
}

async fn file_info(
    client: &reqwest::Client,
    api: &str,
    seq: &AtomicU32,
    link: &Link,
) -> omni_core::Result<FileInfo> {
    match link {
        Link::File { id, key } => {
            let key = FileKey::from_bytes(key)
                .ok_or_else(|| Error::host(NAME, "la clave del enlace no es válida"))?;
            let r = api_call(client, api, seq, None, json!({"a": "g", "g": 1, "p": id})).await?;
            Ok(FileInfo {
                size: r["s"].as_u64().unwrap_or(0),
                name: r["at"]
                    .as_str()
                    .and_then(|a| decrypt_name(&key.aes, a))
                    .unwrap_or_else(|| id.clone()),
                key,
                url: r["g"]
                    .as_str()
                    .ok_or_else(|| Error::host(NAME, "Mega no dio la dirección de descarga"))?
                    .to_owned(),
            })
        }
        Link::Folder {
            id,
            key,
            node: Some(node),
        } => {
            let fkey: [u8; 16] = key
                .as_slice()
                .try_into()
                .map_err(|_| Error::host(NAME, "la clave de la carpeta no es válida"))?;
            let m = Mega::with_api(api);
            let (_, nodes) = m.nodes(client, id, &fkey).await?;
            let n = nodes
                .into_iter()
                .find(|n| &n.id == node && !n.is_folder)
                .ok_or(Error::LinkExpired { status: 404 })?;
            let key = FileKey::from_bytes(&n.key)
                .ok_or_else(|| Error::host(NAME, "clave de archivo no válida"))?;
            let r = api_call(
                client,
                api,
                seq,
                Some(id),
                json!({"a": "g", "g": 1, "n": node}),
            )
            .await?;
            Ok(FileInfo {
                size: r["s"].as_u64().or(n.size).unwrap_or(0),
                name: n.name,
                key,
                url: r["g"]
                    .as_str()
                    .ok_or_else(|| Error::host(NAME, "Mega no dio la dirección de descarga"))?
                    .to_owned(),
            })
        }
        Link::Folder { node: None, .. } => Err(Error::host(
            NAME,
            "es una carpeta: hay que añadir sus archivos",
        )),
    }
}

impl Default for Mega {
    fn default() -> Self {
        Self::new()
    }
}

impl Host for Mega {
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
            let link = parse(url).ok_or_else(|| Error::InvalidUrl(url.to_string()))?;
            match &link {
                Link::Folder {
                    id,
                    key,
                    node: None,
                } => {
                    let fkey: [u8; 16] = key
                        .as_slice()
                        .try_into()
                        .map_err(|_| Error::host(NAME, "la clave de la carpeta no es válida"))?;
                    let (name, nodes) = self.nodes(client, id, &fkey).await?;
                    let files: Vec<FolderFile> = nodes
                        .iter()
                        .filter(|n| !n.is_folder)
                        .filter_map(|n| {
                            Some(FolderFile {
                                url: folder_file_url(id, key, &n.id)?,
                                name: n.name.clone(),
                                size: n.size,
                                resumable: true,
                            })
                        })
                        .collect();
                    if files.is_empty() {
                        return Err(Error::host(NAME, "la carpeta no tiene archivos"));
                    }
                    Ok(Found::Folder { name, files })
                }
                _ => {
                    let info = self.file_info(client, &link).await?;
                    Ok(Found::File(RemoteInfo {
                        url: url.clone(),
                        size: Some(info.size),
                        accepts_ranges: true,
                        etag: None,
                        last_modified: None,
                        filename: omni_core::filename::sanitize(&info.name),
                    }))
                }
            }
        })
    }

    fn resolve<'a>(&'a self, client: &'a reqwest::Client, url: &'a Url) -> ResolveFuture<'a> {
        Box::pin(async move {
            let link = parse(url).ok_or_else(|| Error::InvalidUrl(url.to_string()))?;
            if matches!(link, Link::Folder { node: None, .. }) {
                return Err(Error::host(
                    NAME,
                    "es una carpeta: hay que añadir sus archivos",
                ));
            }
            Ok(ResolvedLink {
                custom: Some(Arc::new(MegaJob {
                    client: client.clone(),
                    api: self.api.clone(),
                    link,
                })),
                ..ResolvedLink::default()
            })
        })
    }
}

// ----------------------------------------------------------------- descarga

struct MegaJob {
    client: reqwest::Client,
    api: String,
    link: Link,
}

impl std::fmt::Debug for MegaJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MegaJob")
    }
}

impl CustomDownload for MegaJob {
    fn run<'a>(
        &'a self,
        dir: &'a Path,
        progress: Arc<Progress>,
        cancel: CancellationToken,
    ) -> RunFuture<'a> {
        Box::pin(async move {
            let seq = AtomicU32::new(7_000_000);
            let mut info = tokio::select! {
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                r = file_info(&self.client, &self.api, &seq, &self.link) => r?,
            };
            std::fs::create_dir_all(dir)?;
            let path = dir.join(omni_core::filename::sanitize(&info.name));
            let part = part_path_for(&path);
            // Seguir donde se quedó, en un límite de bloque de 16 bytes.
            let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
            let mut done = (have.min(info.size) / 16) * 16;
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&part)
                .await?;
            file.set_len(done).await?;
            tokio::io::AsyncSeekExt::seek(&mut file, std::io::SeekFrom::Start(done)).await?;
            progress.report(done, Some(info.size));

            let mut failures = 0;
            let mut renewed = false;
            while done < info.size {
                let url = format!("{}/{done}-{}", info.url, info.size - 1);
                let sent = tokio::select! {
                    _ = cancel.cancelled() => return Err(Error::Cancelled),
                    r = self.client.get(&url).send() => r,
                };
                let mut resp = match sent {
                    Ok(r) => r,
                    Err(e) => {
                        failures += 1;
                        if failures >= STREAM_TRIES {
                            return Err(e.into());
                        }
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                };
                let status = resp.status().as_u16();
                if status == 509 {
                    return Err(api_error(-17));
                }
                if matches!(status, 403 | 404 | 410) {
                    // La dirección temporal caducó: pedir otra una vez.
                    if renewed {
                        return Err(Error::LinkExpired { status });
                    }
                    renewed = true;
                    info.url = file_info(&self.client, &self.api, &seq, &self.link)
                        .await?
                        .url;
                    continue;
                }
                if !resp.status().is_success() {
                    return Err(crate::status_error(resp.status()));
                }
                let mut cipher = ctr_at(&info.key, done);
                loop {
                    let chunk = tokio::select! {
                        _ = cancel.cancelled() => {
                            file.flush().await?;
                            return Err(Error::Cancelled);
                        }
                        c = resp.chunk() => c,
                    };
                    match chunk {
                        Ok(Some(bytes)) => {
                            let mut buf = bytes.to_vec();
                            let room = (info.size - done) as usize;
                            buf.truncate(room);
                            cipher.apply_keystream(&mut buf);
                            file.write_all(&buf).await?;
                            done += buf.len() as u64;
                            failures = 0;
                            progress.report(done, Some(info.size));
                            if done >= info.size {
                                break;
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            failures += 1;
                            if failures >= STREAM_TRIES {
                                return Err(e.into());
                            }
                            break;
                        }
                    }
                }
                // Un corte a mitad: seguir desde un límite de 16 bytes.
                if done < info.size {
                    let aligned = (done / 16) * 16;
                    if aligned != done {
                        file.flush().await?;
                        file.set_len(aligned).await?;
                        tokio::io::AsyncSeekExt::seek(&mut file, std::io::SeekFrom::Start(aligned))
                            .await?;
                        done = aligned;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
            file.flush().await?;
            file.sync_all().await?;
            drop(file);
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
            std::fs::rename(&part, &path)?;
            Ok(DownloadOutcome {
                path,
                size: info.size,
                resumed_from: 0,
                segmented: true,
                sha256: None,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(s: &str) -> Option<Link> {
        parse(&Url::parse(s).unwrap())
    }

    #[test]
    fn recognises_links() {
        let key32 = b64_encode(&[7u8; 32]);
        let key16 = b64_encode(&[9u8; 16]);
        assert_eq!(
            link(&format!("https://mega.nz/file/AbCd1234#{key32}")),
            Some(Link::File {
                id: "AbCd1234".into(),
                key: vec![7; 32]
            })
        );
        assert_eq!(
            link(&format!("https://mega.nz/#!AbCd1234!{key32}")),
            Some(Link::File {
                id: "AbCd1234".into(),
                key: vec![7; 32]
            })
        );
        assert_eq!(
            link(&format!("https://mega.nz/folder/Fold1234#{key16}")),
            Some(Link::Folder {
                id: "Fold1234".into(),
                key: vec![9; 16],
                node: None
            })
        );
        assert_eq!(
            link(&format!(
                "https://mega.nz/folder/Fold1234#{key16}/file/Nod12345"
            )),
            Some(Link::Folder {
                id: "Fold1234".into(),
                key: vec![9; 16],
                node: Some("Nod12345".into())
            })
        );
        assert_eq!(
            link(&format!("https://mega.nz/#F!Fold1234!{key16}")),
            Some(Link::Folder {
                id: "Fold1234".into(),
                key: vec![9; 16],
                node: None
            })
        );
        // Sin clave no es un enlace útil.
        assert_eq!(link("https://mega.nz/file/AbCd1234"), None);
        assert_eq!(link("https://mega.nz/login"), None);
        assert_eq!(link("https://example.com/file/x#y"), None);
    }

    #[test]
    fn key_combines_halves() {
        let mut k = [0u8; 32];
        for (i, b) in k.iter_mut().enumerate() {
            *b = i as u8;
        }
        let fk = FileKey::from_bytes(&k).unwrap();
        assert_eq!(fk.aes[0], 16);
        assert_eq!(fk.aes[15], 15 ^ 31);
        assert_eq!(fk.nonce, [16, 17, 18, 19, 20, 21, 22, 23]);
        assert!(FileKey::from_bytes(&k[..16]).is_none());
    }

    #[test]
    fn ctr_can_start_mid_stream() {
        let key = FileKey {
            aes: [3; 16],
            nonce: [5; 8],
        };
        let mut all = vec![0u8; 100];
        ctr_at(&key, 0).apply_keystream(&mut all);
        let mut tail = vec![0u8; 100 - 48];
        ctr_at(&key, 48).apply_keystream(&mut tail);
        assert_eq!(&all[48..], &tail[..]);
    }
}
