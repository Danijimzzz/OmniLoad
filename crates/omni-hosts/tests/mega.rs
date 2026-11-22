//! Mega contra una API de mentira que cifra como Mega (la clave de 256 bits
//! con XOR, nombres en AES-CBC, contenido en AES-CTR y claves de carpeta en
//! AES-ECB), para probar enlaces, carpetas, descifrado y reanudación.

use std::sync::Arc;
use std::time::Duration;

use aes::Aes128;
use aes::cipher::generic_array::GenericArray;
use aes::cipher::{
    BlockEncrypt, BlockEncryptMut, KeyInit, KeyIvInit, StreamCipher, block_padding::NoPadding,
};
use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use omni_core::{Error, Network, Progress};
use omni_grab::Found;
use omni_hosts::{Hosts, Mega};
use omni_queue::Resolver;
use omni_testserver::pseudo_random_bytes;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// Una clave de archivo de 32 bytes con ese AES y ese nonce.
fn file_key(aes: [u8; 16], nonce: [u8; 8]) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[16..24].copy_from_slice(&nonce);
    k[24..32].copy_from_slice(&[0xAA; 8]); // "MAC"
    for i in 0..16 {
        k[i] = aes[i] ^ k[16 + i];
    }
    k
}

fn encrypt_attr(key: &[u8; 16], name: &str) -> String {
    let mut data = format!(r#"MEGA{{"n":"{name}"}}"#).into_bytes();
    let len = data.len();
    data.resize(len.div_ceil(16) * 16, 0);
    let enc = cbc::Encryptor::<Aes128>::new(key.into(), &[0u8; 16].into());
    let n = data.len();
    enc.encrypt_padded_mut::<NoPadding>(&mut data, n).unwrap();
    b64(&data)
}

fn encrypt_content(aes: [u8; 16], nonce: [u8; 8], plain: &[u8]) -> Vec<u8> {
    let mut iv = [0u8; 16];
    iv[..8].copy_from_slice(&nonce);
    let mut c = ctr::Ctr128BE::<Aes128>::new((&aes).into(), (&iv).into());
    let mut out = plain.to_vec();
    c.apply_keystream(&mut out);
    out
}

fn ecb_encrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let c = Aes128::new(GenericArray::from_slice(key));
    let mut out = data.to_vec();
    for b in out.chunks_exact_mut(16) {
        c.encrypt_block(GenericArray::from_mut_slice(b));
    }
    out
}

struct Fixture {
    base: String,
    /// Texto claro del archivo.
    plain: Vec<u8>,
    file_link: String,
    folder_link: String,
}

#[derive(Clone)]
struct St {
    base: Arc<std::sync::OnceLock<String>>,
    cipher: Arc<Vec<u8>>,
    file_aes: [u8; 16],
    folder_key: [u8; 16],
    node_k32: [u8; 32],
    node_aes: [u8; 16],
}

async fn api(
    State(st): State<St>,
    Query(q): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<Vec<Value>>,
) -> Json<Value> {
    let cmd = &body[0];
    let base = st.base.get().unwrap();
    let size = st.cipher.len();
    Json(match (cmd["a"].as_str(), cmd["p"].as_str(), q.get("n")) {
        (Some("g"), Some("quota"), _) => json!([-17]),
        (Some("g"), Some("gone"), _) => json!([-9]),
        (Some("g"), Some("FILE1234"), None) => json!([{
            "s": size,
            "at": encrypt_attr(&st.file_aes, "video de prueba.mp4"),
            "g": format!("{base}/dl/file")
        }]),
        (Some("f"), _, Some(_)) => json!([{"f": [
            {"h": "FOLD1234", "t": 2, "a": encrypt_attr(&st.folder_key, "Carpeta Mega")},
            {"h": "NODE0001", "t": 0, "p": "FOLD1234", "s": size,
             "a": encrypt_attr(&st.node_aes, "dentro.bin"),
             "k": format!("FOLD1234:{}", b64(&ecb_encrypt(&st.folder_key, &st.node_k32)))},
            {"h": "NODE0002", "t": 1, "p": "FOLD1234",
             "a": encrypt_attr(&st.folder_key, "subcarpeta"),
             "k": format!("FOLD1234:{}", b64(&ecb_encrypt(&st.folder_key, &[1u8; 16])))}
        ]}]),
        (Some("g"), _, Some(_)) => json!([{
            "s": size,
            "g": format!("{base}/dl/node")
        }]),
        _ => json!([-2]),
    })
}

async fn fixture() -> Fixture {
    let plain = pseudo_random_bytes(200_003, 11);
    let aes = [0x11u8; 16];
    let nonce = [0x22u8; 8];
    let k32 = file_key(aes, nonce);
    let cipher = encrypt_content(aes, nonce, &plain);

    let folder_key = [0x33u8; 16];
    let node_aes_src = [0x44u8; 16];
    let node_nonce = [0x55u8; 8];
    let node_k32 = file_key(node_aes_src, node_nonce);
    // El texto del nodo de carpeta usa su propio contenido.
    let node_cipher = encrypt_content(node_aes_src, node_nonce, &plain);

    let st = St {
        base: Arc::default(),
        cipher: Arc::new(node_cipher.clone()),
        file_aes: aes,
        folder_key,
        node_k32,
        node_aes: node_aes_src,
    };
    let file_cipher = Arc::new(cipher);
    let node_cipher = Arc::new(node_cipher);
    let base_cell = st.base.clone();
    let st_file = st.clone();
    // El archivo suelto tiene su propio contenido cifrado.
    let st_file = St {
        cipher: file_cipher.clone(),
        ..st_file
    };
    let app = Router::new()
        .route("/cs", post(api).with_state(st_file.clone()))
        .route(
            "/dl/{what}/{range}",
            get(move |Path((what, range)): Path<(String, String)>| {
                let file_cipher = file_cipher.clone();
                let node_cipher = node_cipher.clone();
                async move {
                    let (a, b) = range.split_once('-').unwrap();
                    let (a, b): (usize, usize) = (a.parse().unwrap(), b.parse().unwrap());
                    let data = if what == "file" {
                        file_cipher
                    } else {
                        node_cipher
                    };
                    data[a..=b.min(data.len() - 1)].to_vec()
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    base_cell.set(base.clone()).unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Fixture {
        base,
        plain,
        file_link: format!("https://mega.nz/file/FILE1234#{}", b64(&k32)),
        folder_link: format!("https://mega.nz/folder/FOLD1234#{}", b64(&folder_key)),
    }
}

fn client() -> reqwest::Client {
    Network::system_default().unwrap().client().clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_link_check_and_download() {
    let fx = fixture().await;
    let hosts = Hosts::empty().with(Mega::with_api(&fx.base));
    let url = Url::parse(&fx.file_link).unwrap();

    let Found::File(info) = hosts.checkers().check(&client(), &url).await.unwrap() else {
        panic!("se esperaba un archivo")
    };
    assert_eq!(info.filename, "video de prueba.mp4");
    assert_eq!(info.size, Some(fx.plain.len() as u64));

    let tmp = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let link = hosts
        .resolver(client())
        .resolve(&fx.file_link, &cancel)
        .await
        .unwrap();
    let progress = Arc::new(Progress::default());
    let done = link
        .custom
        .unwrap()
        .run(tmp.path(), progress.clone(), cancel)
        .await
        .unwrap();
    assert_eq!(done.path.file_name().unwrap(), "video de prueba.mp4");
    assert_eq!(std::fs::read(&done.path).unwrap(), fx.plain);
    assert_eq!(progress.downloaded(), fx.plain.len() as u64);
    assert!(!tmp.path().join("video de prueba.mp4.part").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumes_from_a_partial_file() {
    let fx = fixture().await;
    let hosts = Hosts::empty().with(Mega::with_api(&fx.base));
    let tmp = tempfile::tempdir().unwrap();
    // Una descarga a medias: 100.003 bytes ya descifrados (no múltiplo de 16).
    let part = omni_core::part_path_for(&tmp.path().join("video de prueba.mp4"));
    std::fs::write(&part, &fx.plain[..100_003]).unwrap();
    let cancel = CancellationToken::new();
    let link = hosts
        .resolver(client())
        .resolve(&fx.file_link, &cancel)
        .await
        .unwrap();
    let done = link
        .custom
        .unwrap()
        .run(tmp.path(), Arc::new(Progress::default()), cancel)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&done.path).unwrap(), fx.plain);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn folders_and_errors() {
    let fx = fixture().await;
    let hosts = Hosts::empty().with(Mega::with_api(&fx.base));
    let url = Url::parse(&fx.folder_link).unwrap();

    let Found::Folder { name, files } = hosts.checkers().check(&client(), &url).await.unwrap()
    else {
        panic!("se esperaba una carpeta")
    };
    assert_eq!(name.as_deref(), Some("Carpeta Mega"));
    assert_eq!(files.len(), 1, "la subcarpeta no es un archivo");
    assert_eq!(files[0].name, "dentro.bin");
    assert!(files[0].url.as_str().ends_with("/file/NODE0001"));

    // Un archivo de la carpeta se resuelve y se descarga con su clave.
    let tmp = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let link = hosts
        .resolver(client())
        .resolve(files[0].url.as_str(), &cancel)
        .await
        .unwrap();
    let done = link
        .custom
        .unwrap()
        .run(tmp.path(), Arc::new(Progress::default()), cancel)
        .await
        .unwrap();
    assert_eq!(done.path.file_name().unwrap(), "dentro.bin");
    assert_eq!(std::fs::read(&done.path).unwrap(), fx.plain);

    // Errores de la API, en claro.
    let key = b64(&[9u8; 32]);
    let err = hosts
        .checkers()
        .check(
            &client(),
            &Url::parse(&format!("https://mega.nz/file/quota#{key}")).unwrap(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("cuota"), "{err}");
    let err = hosts
        .checkers()
        .check(
            &client(),
            &Url::parse(&format!("https://mega.nz/file/gone#{key}")).unwrap(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::LinkExpired { status: 404 }), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_keeps_the_partial_file() {
    let fx = fixture().await;
    let hosts = Hosts::empty().with(Mega::with_api(&fx.base));
    let tmp = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let link = hosts
        .resolver(client())
        .resolve(&fx.file_link, &cancel)
        .await
        .unwrap();
    cancel.cancel();
    let err = tokio::time::timeout(
        Duration::from_secs(10),
        link.custom
            .unwrap()
            .run(tmp.path(), Arc::new(Progress::default()), cancel),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(matches!(err, Error::Cancelled), "{err}");
}
