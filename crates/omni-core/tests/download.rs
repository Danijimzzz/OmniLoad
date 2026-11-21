//! Pruebas de extremo a extremo contra el servidor falso.

use std::path::Path;
use std::time::Duration;

use omni_core::{Destination, Download, DownloadOptions, Error, state_path_for};
use omni_testserver::{ServerConfig, TestServer, pseudo_random_bytes};
use tokio_util::sync::CancellationToken;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;

/// Opciones rápidas para pruebas: bloques pequeños y esperas cortas.
fn fast_opts() -> DownloadOptions {
    DownloadOptions {
        connections_per_network: 4,
        block_size: 256 * KIB,
        min_block_size: 64 * KIB,
        write_buffer: 64 * 1024,
        retry_base: Duration::from_millis(10),
        retry_max: Duration::from_millis(100),
        save_interval: Duration::from_millis(50),
        ..DownloadOptions::default()
    }
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

#[tokio::test]
async fn multipart_download_matches() {
    let data = pseudo_random_bytes((3 * MIB + 12_345) as usize, 1);
    // Una pausa mínima entre trozos: sin ella, en localhost el servidor llena
    // los búferes del sistema con bytes que el cliente ya no quiere (al partir
    // bloques en el final rápido) y la cuenta de bytes servidos sale inflada.
    let cfg = ServerConfig {
        chunk_delay: Some(Duration::from_millis(1)),
        ..ServerConfig::default()
    };
    let server = TestServer::start(data.clone(), cfg).await;
    let dir = tempfile::tempdir().unwrap();

    let dl = Download::new(
        &server.url("archivo.bin"),
        Destination::Dir(dir.path().into()),
        fast_opts(),
    )
    .unwrap();
    let out = dl.run(CancellationToken::new()).await.unwrap();

    assert_eq!(out.path, dir.path().join("archivo.bin"));
    assert!(out.segmented);
    assert_eq!(read(&out.path), data);
    // Varias peticiones con rango, sin descargar nada dos veces de más.
    assert!(server.ranges().len() > 4);
    let served = server.bytes_served();
    assert!(
        served < data.len() as u64 + 2 * 256 * KIB,
        "se sirvieron {served} bytes para un archivo de {}",
        data.len()
    );
    assert!(!state_path_for(&out.path).exists());
    assert!(!dir.path().join("archivo.bin.part").exists());
}

#[tokio::test]
async fn small_file_and_content_disposition() {
    let data = b"hola".to_vec();
    let cfg = ServerConfig {
        content_disposition: Some("attachment; filename*=UTF-8''pel%C3%ADcula.txt".into()),
        ..ServerConfig::default()
    };
    let server = TestServer::start(data.clone(), cfg).await;
    let dir = tempfile::tempdir().unwrap();
    let out = Download::new(
        &server.url("x"),
        Destination::Dir(dir.path().into()),
        fast_opts(),
    )
    .unwrap()
    .run(CancellationToken::new())
    .await
    .unwrap();
    assert_eq!(out.path, dir.path().join("película.txt"));
    assert_eq!(read(&out.path), data);
}

#[tokio::test]
async fn empty_file() {
    let server = TestServer::start(Vec::new(), ServerConfig::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let out = Download::new(
        &server.url("vacio"),
        Destination::Dir(dir.path().into()),
        fast_opts(),
    )
    .unwrap()
    .run(CancellationToken::new())
    .await
    .unwrap();
    assert_eq!(out.size, 0);
    assert!(read(&out.path).is_empty());
}

#[tokio::test]
async fn server_without_ranges_uses_single_stream() {
    let data = pseudo_random_bytes((MIB + 7) as usize, 2);
    let cfg = ServerConfig {
        ranges: false,
        ..ServerConfig::default()
    };
    let server = TestServer::start(data.clone(), cfg).await;
    let dir = tempfile::tempdir().unwrap();
    let out = Download::new(
        &server.url("a.bin"),
        Destination::Dir(dir.path().into()),
        fast_opts(),
    )
    .unwrap()
    .run(CancellationToken::new())
    .await
    .unwrap();
    assert!(!out.segmented);
    assert_eq!(read(&out.path), data);
}

#[tokio::test]
async fn survives_cut_connections() {
    let data = pseudo_random_bytes((2 * MIB) as usize, 3);
    let cfg = ServerConfig {
        cuts: 6,
        cut_after: 40 * KIB,
        // Con pausas, hyper envía cada trozo antes del corte; sin ellas
        // descarta su búfer y el cliente no llega a recibir nada.
        chunk_delay: Some(Duration::from_millis(1)),
        ..ServerConfig::default()
    };
    let server = TestServer::start(data.clone(), cfg).await;
    let dir = tempfile::tempdir().unwrap();
    let out = Download::new(
        &server.url("a.bin"),
        Destination::Dir(dir.path().into()),
        fast_opts(),
    )
    .unwrap()
    .run(CancellationToken::new())
    .await
    .unwrap();
    assert_eq!(read(&out.path), data);
    // Tras un corte se pide solo lo que faltaba, no el bloque entero.
    let resumed_mid_block = server
        .ranges()
        .iter()
        .any(|(s, _)| s % (256 * KIB) != 0 && s % (64 * KIB) != 0);
    assert!(resumed_mid_block, "rangos: {:?}", server.ranges());
}

#[tokio::test]
async fn retries_503() {
    let data = pseudo_random_bytes((MIB) as usize, 4);
    let cfg = ServerConfig {
        fail_first: 5,
        ..ServerConfig::default()
    };
    let server = TestServer::start(data.clone(), cfg).await;
    let dir = tempfile::tempdir().unwrap();
    let out = Download::new(
        &server.url("a.bin"),
        Destination::Dir(dir.path().into()),
        fast_opts(),
    )
    .unwrap()
    .run(CancellationToken::new())
    .await
    .unwrap();
    assert_eq!(read(&out.path), data);
}

#[tokio::test]
async fn gives_up_after_max_retries() {
    let cfg = ServerConfig {
        fail_first: 1000,
        ..ServerConfig::default()
    };
    let server = TestServer::start(vec![1; 1000], cfg).await;
    let dir = tempfile::tempdir().unwrap();
    let opts = DownloadOptions {
        max_retries: 2,
        ..fast_opts()
    };
    let err = Download::new(
        &server.url("a.bin"),
        Destination::Dir(dir.path().into()),
        opts,
    )
    .unwrap()
    .run(CancellationToken::new())
    .await
    .unwrap_err();
    assert!(matches!(err, Error::RetriesExhausted(_)), "{err}");
}

/// Lanza una descarga lenta y la cancela cuando lleva al menos `min_bytes`.
async fn download_until_cancelled(server: &TestServer, dest: &Path, min_bytes: u64) -> u64 {
    let dl = Download::new(
        &server.url("a.bin"),
        Destination::File(dest.into()),
        fast_opts(),
    )
    .unwrap();
    let progress = dl.progress();
    let cancel = CancellationToken::new();
    let task = tokio::spawn(dl.run(cancel.clone()));
    while progress.downloaded() < min_bytes {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    cancel.cancel();
    let err = task.await.unwrap().unwrap_err();
    assert!(matches!(err, Error::Cancelled), "{err}");
    progress.downloaded()
}

#[tokio::test]
async fn resumes_after_cancel() {
    let data = pseudo_random_bytes((2 * MIB) as usize, 5);
    let cfg = ServerConfig {
        chunk_delay: Some(Duration::from_millis(5)),
        ..ServerConfig::default()
    };
    let server = TestServer::start(data.clone(), cfg).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("a.bin");

    let saved = download_until_cancelled(&server, &dest, 512 * KIB).await;
    assert!(state_path_for(&dest).exists());
    let served_first = server.bytes_served();

    server.set_chunk_delay(None);
    let out = Download::new(
        &server.url("a.bin"),
        Destination::File(dest.clone()),
        fast_opts(),
    )
    .unwrap()
    .run(CancellationToken::new())
    .await
    .unwrap();
    assert_eq!(out.resumed_from, saved);
    assert_eq!(read(&dest), data);
    // La segunda vez solo se pidió lo que faltaba.
    let served_second = server.bytes_served() - served_first;
    assert!(
        served_second <= data.len() as u64 - saved + 4 * 64 * KIB,
        "segunda vez: {served_second}, guardado: {saved}"
    );
}

#[tokio::test]
async fn restarts_when_etag_changes() {
    let old = pseudo_random_bytes((2 * MIB) as usize, 6);
    let new = pseudo_random_bytes((2 * MIB) as usize, 7);
    let cfg = ServerConfig {
        chunk_delay: Some(Duration::from_millis(5)),
        ..ServerConfig::default()
    };
    let server = TestServer::start(old, cfg).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("a.bin");

    download_until_cancelled(&server, &dest, 256 * KIB).await;

    server.set_content(new.clone(), "v2");
    server.set_chunk_delay(None);
    let out = Download::new(
        &server.url("a.bin"),
        Destination::File(dest.clone()),
        fast_opts(),
    )
    .unwrap()
    .run(CancellationToken::new())
    .await
    .unwrap();
    assert_eq!(out.resumed_from, 0);
    assert_eq!(read(&dest), new);
}

#[tokio::test]
async fn detects_change_during_download() {
    let old = pseudo_random_bytes((2 * MIB) as usize, 8);
    let cfg = ServerConfig {
        chunk_delay: Some(Duration::from_millis(5)),
        ..ServerConfig::default()
    };
    let server = TestServer::start(old, cfg).await;
    let dir = tempfile::tempdir().unwrap();
    let dl = Download::new(
        &server.url("a.bin"),
        Destination::Dir(dir.path().into()),
        fast_opts(),
    )
    .unwrap();
    let progress = dl.progress();
    let task = tokio::spawn(dl.run(CancellationToken::new()));
    while progress.downloaded() < 128 * KIB {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    server.set_content(pseudo_random_bytes((2 * MIB) as usize, 9), "v2");
    let err = task.await.unwrap().unwrap_err();
    assert!(matches!(err, Error::RemoteChanged), "{err}");
}

#[tokio::test]
async fn checks_sha256() {
    let data = b"contenido de prueba".to_vec();
    let server = TestServer::start(data, ServerConfig::default()).await;
    let dir = tempfile::tempdir().unwrap();

    let opts = DownloadOptions {
        expected_sha256: Some("00".repeat(32)),
        ..fast_opts()
    };
    let err = Download::new(
        &server.url("a.txt"),
        Destination::Dir(dir.path().into()),
        opts,
    )
    .unwrap()
    .run(CancellationToken::new())
    .await
    .unwrap_err();
    let Error::HashMismatch { actual, .. } = err else {
        panic!("se esperaba HashMismatch, llegó {err}");
    };

    let opts = DownloadOptions {
        expected_sha256: Some(actual.to_uppercase()),
        overwrite: true,
        ..fast_opts()
    };
    let out = Download::new(
        &server.url("a.txt"),
        Destination::Dir(dir.path().into()),
        opts,
    )
    .unwrap()
    .run(CancellationToken::new())
    .await
    .unwrap();
    assert_eq!(out.sha256.as_deref(), Some(actual.as_str()));
}

#[tokio::test]
async fn refuses_to_overwrite() {
    let server = TestServer::start(b"x".to_vec(), ServerConfig::default()).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), b"mio").unwrap();
    let err = Download::new(
        &server.url("a.txt"),
        Destination::Dir(dir.path().into()),
        fast_opts(),
    )
    .unwrap()
    .run(CancellationToken::new())
    .await
    .unwrap_err();
    assert!(matches!(err, Error::AlreadyExists(_)));
    assert_eq!(read(&dir.path().join("a.txt")), b"mio");
}

#[tokio::test]
async fn rejects_non_http_urls() {
    let err = Download::new("ftp://x/y", Destination::Dir(".".into()), fast_opts())
        .err()
        .unwrap();
    assert!(matches!(err, Error::InvalidUrl(_)));
}
