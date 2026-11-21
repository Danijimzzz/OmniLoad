// Benchmark temporal: velocidad por segundo de una descarga grande.
use omni_core::{Destination, Download, DownloadOptions};
use omni_testserver::{ServerConfig, TestServer, pseudo_random_bytes};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() {
    let mb: usize = std::env::args()
        .nth(1)
        .map(|s| s.parse().unwrap())
        .unwrap_or(512);
    let delay_us: u64 = std::env::args()
        .nth(2)
        .map(|s| s.parse().unwrap())
        .unwrap_or(0);
    let cfg = ServerConfig {
        chunk: 64 * 1024,
        chunk_delay: (delay_us > 0).then(|| Duration::from_micros(delay_us)),
        ..Default::default()
    };
    let server = TestServer::start(pseudo_random_bytes(mb << 20, 7), cfg).await;
    let dir = std::env::args()
        .nth(3)
        .unwrap_or("/tmp/claude-0/bench".into());
    std::fs::create_dir_all(&dir).unwrap();
    let dl = Download::new(
        &server.url("big.bin"),
        Destination::Dir(dir.into()),
        DownloadOptions {
            overwrite: true,
            ..Default::default()
        },
    )
    .unwrap();
    let p = dl.progress();
    let t = tokio::spawn(dl.run(CancellationToken::new()));
    let start = Instant::now();
    let mut last = 0;
    while !t.is_finished() {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let d = p.downloaded();
        let conns: usize = p.per_network().values().map(|s| s.connections).sum();
        println!(
            "{:5.1}s {:7.1} MiB/s  {:5} MiB  conns={}",
            start.elapsed().as_secs_f64(),
            (d - last) as f64 / 0.5 / 1048576.0,
            d >> 20,
            conns
        );
        last = d;
    }
    t.await.unwrap().unwrap();
    println!("total {:.1}s", start.elapsed().as_secs_f64());
}
