//! Banco de pruebas: velocidad, conexiones y límite cada medio segundo durante
//! una descarga grande contra el servidor falso.
//!
//! ```text
//! cargo run --release -p omni-testserver --example bench -- [MiB] [pausa_µs] [carpeta]
//! MAXC=5 cargo run ...   # servidor que solo acepta 5 conexiones a la vez
//! ```

use std::time::{Duration, Instant};

use omni_core::{Destination, Download, DownloadOptions};
use omni_testserver::{ServerConfig, TestServer, pseudo_random_bytes};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() {
    let arg = |i: usize| std::env::args().nth(i);
    let mib: usize = arg(1).map_or(512, |s| s.parse().expect("MiB"));
    let delay_us: u64 = arg(2).map_or(0, |s| s.parse().expect("pausa"));
    let dir = arg(3).unwrap_or_else(|| {
        std::env::temp_dir()
            .join("omni-bench")
            .to_string_lossy()
            .into_owned()
    });
    let max_concurrent = std::env::var("MAXC").ok().map(|v| v.parse().expect("MAXC"));

    let cfg = ServerConfig {
        chunk: 16 * 1024,
        chunk_delay: (delay_us > 0).then(|| Duration::from_micros(delay_us)),
        max_concurrent,
        ..Default::default()
    };
    let server = TestServer::start(pseudo_random_bytes(mib << 20, 7), cfg).await;
    std::fs::create_dir_all(&dir).expect("carpeta");
    let dl = Download::new(
        &server.url("big.bin"),
        Destination::Dir(dir.into()),
        DownloadOptions {
            overwrite: true,
            ..Default::default()
        },
    )
    .expect("descarga");
    let progress = dl.progress();
    let task = tokio::spawn(dl.run(CancellationToken::new()));
    let start = Instant::now();
    let mut last = 0;
    while !task.is_finished() {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let done = progress.downloaded();
        let st = progress
            .per_network()
            .values()
            .next()
            .copied()
            .unwrap_or_default();
        println!(
            "{:5.1}s {:7.1} MiB/s {:5} MiB  recibiendo={} límite={} reintentos={}",
            start.elapsed().as_secs_f64(),
            (done - last) as f64 / 0.5 / 1048576.0,
            done >> 20,
            st.receiving,
            st.limit,
            st.retries
        );
        last = done;
    }
    task.await.expect("tarea").expect("descarga");
    println!(
        "total {:.1}s, {} peticiones, {} rechazos 429",
        start.elapsed().as_secs_f64(),
        server.ranges().len(),
        server.rejected()
    );
}
