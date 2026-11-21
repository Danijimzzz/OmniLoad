//! Criterio de la fase 1: matar el proceso a lo bruto (SIGKILL en Unix,
//! TerminateProcess en Windows) no pierde más de 1 s de progreso, y al volver a
//! lanzar el mismo comando la descarga se reanuda y el archivo sale idéntico.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use omni_testserver::{ServerConfig, TestServer, pseudo_random_bytes};

const KIB: u64 = 1024;

fn saved_bytes(state: &Path) -> Option<u64> {
    let data = std::fs::read(state).ok()?;
    let json: serde_json::Value = serde_json::from_slice(&data).ok()?;
    Some(
        json["blocks"]
            .as_array()?
            .iter()
            .map(|b| b["written"].as_u64().unwrap_or(0))
            .sum(),
    )
}

fn omni(url: &str, dest: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_omni"));
    cmd.args(["get", url, "-q", "-x", "4", "--block-size", "256K", "-o"])
        .arg(dest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    cmd
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_and_resume() {
    // 4 conexiones × 16 KiB cada 50 ms ≈ 1,25 MiB/s en total.
    let chunk = 16 * KIB;
    let delay = Duration::from_millis(50);
    let connections = 4;
    let rate_per_sec = connections * chunk * 1000 / delay.as_millis() as u64;

    let data = pseudo_random_bytes((4 * 1024 * KIB) as usize, 42);
    let cfg = ServerConfig {
        chunk: chunk as usize,
        chunk_delay: Some(delay),
        ..ServerConfig::default()
    };
    let server = TestServer::start(data.clone(), cfg).await;
    let url = server.url("grande.bin");
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("grande.bin");
    let state = omni_core::state_path_for(&dest);

    let mut child = omni(&url, &dest).spawn().unwrap();
    let start = Instant::now();
    while saved_bytes(&state).unwrap_or(0) < 1024 * KIB {
        assert!(start.elapsed() < Duration::from_secs(30), "no avanza");
        assert!(
            child.try_wait().unwrap().is_none(),
            "terminó antes de tiempo"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    child.kill().unwrap();
    child.wait().unwrap();
    // Dejar que el servidor note el cierre.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let served = server.bytes_served();
    let saved = saved_bytes(&state).expect("estado guardado");
    let lost = served.saturating_sub(saved);
    assert!(saved > 0 && saved < data.len() as u64);
    assert!(
        lost <= rate_per_sec,
        "se perdieron {lost} bytes, más de 1 s de descarga ({rate_per_sec} bytes)"
    );

    // Segunda ejecución. Seguimos frenando el servidor: sin freno, en
    // localhost el servidor llena los búferes del sistema con bytes que el
    // cliente ya no quiere (al partir bloques) y la cuenta sale inflada.
    let status = omni(&url, &dest).status().unwrap();
    assert!(status.success());
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert!(!state.exists());
    let second = server.bytes_served() - served;
    assert!(
        second <= data.len() as u64 - saved + connections * 64 * KIB,
        "la segunda ejecución bajó {second} bytes; debía bajar unos {}",
        data.len() as u64 - saved
    );
}
