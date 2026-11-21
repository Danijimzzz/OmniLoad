//! `omni check` comprueba enlaces sin descargarlos (y sin servicio).

use std::process::Command;

use omni_testserver::{ServerConfig, TestServer, pseudo_random_bytes};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_links_and_files() {
    let server = TestServer::start(pseudo_random_bytes(2048, 9), ServerConfig::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let list = dir.path().join("lista.txt");
    std::fs::write(&list, format!("{}\n", server.url("de-lista.zip"))).unwrap();

    let ok = server.url("bueno.iso");
    let out = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_omni"))
            .args(["check", &ok, list.to_str().unwrap()])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("bueno.iso\t2.00 KiB\tdisponible"), "{text}");
    assert!(text.contains("de-lista.zip"), "{text}");

    // Un enlace caído: sale con error.
    let out = tokio::task::spawn_blocking(|| {
        Command::new(env!("CARGO_BIN_EXE_omni"))
            .args(["check", "http://127.0.0.1:1/caido.zip"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("no disponible"));
}
