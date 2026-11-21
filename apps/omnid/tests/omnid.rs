//! El programa `omnid` de verdad: se lanza en segundo plano, descarga, sigue
//! tras pararlo y volverlo a lanzar, y se para por la API.

use std::path::Path;
use std::time::{Duration, Instant};

use omni_api::{Client, Paths, connect_or_launch};
use omni_queue::{NewJob, State};
use omni_testserver::{ServerConfig, TestServer, pseudo_random_bytes};

async fn wait_until_gone(paths: &Paths) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Client::connect(paths).await.is_some() || paths.endpoint.exists() {
        assert!(Instant::now() < deadline, "omnid no se paró");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn launches_downloads_and_resumes_after_restart() {
    let data = pseudo_random_bytes(4 * 1024 * 1024, 41);
    let cfg = ServerConfig {
        chunk: 16 * 1024,
        chunk_delay: Some(Duration::from_millis(10)),
        ..ServerConfig::default()
    };
    let server = TestServer::start(data.clone(), cfg).await;
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::for_db(dir.path().join("omniload.db"));
    let exe = Path::new(env!("CARGO_BIN_EXE_omnid"));
    let downloads = dir.path().join("descargas");

    let c = connect_or_launch(&paths, Some(exe), Duration::from_secs(20))
        .await
        .expect("omnid arranca");
    let id = c
        .add(&NewJob::new(server.url("grande.bin"), &downloads))
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let j = c.jobs().await.unwrap().remove(0);
        if j.downloaded > 256 * 1024 {
            break;
        }
        assert!(Instant::now() < deadline, "{j:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Parar el servicio a mitad: guarda el progreso.
    c.shutdown().await.unwrap();
    wait_until_gone(&paths).await;

    // Volver a lanzarlo: la descarga sigue sola.
    server.set_chunk_delay(None);
    let c = connect_or_launch(&paths, Some(exe), Duration::from_secs(20))
        .await
        .expect("omnid vuelve a arrancar");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let j = c
            .jobs()
            .await
            .unwrap()
            .into_iter()
            .find(|j| j.id == id)
            .unwrap();
        if j.state == State::Done {
            break;
        }
        assert!(Instant::now() < deadline, "{j:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(std::fs::read(downloads.join("grande.bin")).unwrap(), data);
    c.shutdown().await.unwrap();
    wait_until_gone(&paths).await;
    let log = std::fs::read_to_string(&paths.log).unwrap();
    assert!(log.contains("servicio en marcha"), "{log}");
}
