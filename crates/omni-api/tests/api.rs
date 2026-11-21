//! Pruebas del servicio y sus clientes.

use std::time::{Duration, Instant};

use omni_api::{Client, Endpoint, Paths, RemoteQueue, Service, ServiceError};
use omni_core::DownloadOptions;
use omni_queue::{JobInfo, NewJob, Priority, State};
use omni_testserver::{ServerConfig, TestServer, pseudo_random_bytes};

fn fast_opts() -> DownloadOptions {
    DownloadOptions {
        block_size: 64 * 1024,
        min_block_size: 16 * 1024,
        write_buffer: 16 * 1024,
        retry_base: Duration::from_millis(10),
        retry_max: Duration::from_millis(100),
        save_interval: Duration::from_millis(50),
        ..DownloadOptions::default()
    }
}

async fn wait_job(c: &Client, id: u64, want: State) -> JobInfo {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let job = c
            .jobs()
            .await
            .unwrap()
            .into_iter()
            .find(|j| j.id == id)
            .unwrap();
        if job.state == want {
            return job;
        }
        assert!(Instant::now() < deadline, "no llegó a {want:?}: {job:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_downloads_and_answers() {
    let data = pseudo_random_bytes(512 * 1024, 31);
    let server = TestServer::start(data.clone(), ServerConfig::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::for_db(dir.path().join("datos").join("omniload.db"));
    let service = Service::start(&paths, fast_opts()).await.unwrap();

    // Solo un servicio por base de datos.
    assert!(matches!(
        Service::start(&paths, fast_opts()).await,
        Err(ServiceError::AlreadyRunning)
    ));

    let c = Client::connect(&paths).await.expect("conecta");
    let status = c.status().await.unwrap();
    assert_eq!(status.pid, std::process::id());

    let downloads = dir.path().join("descargas");
    let mut new = NewJob::new(server.url("a.bin"), &downloads);
    new.package = "Paquete".into();
    let id = c.add(&new).await.unwrap();
    let done = wait_job(&c, id, State::Done).await;
    assert_eq!(done.package, "Paquete");
    assert_eq!(std::fs::read(downloads.join("a.bin")).unwrap(), data);

    // Un enlace malo da un error legible.
    let err = c
        .add(&NewJob::new("ftp://x/y", &downloads))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("http"), "{err}");

    // Ajustes.
    let mut s = c.settings().await.unwrap();
    s.max_active = 5;
    s.speed_limit = Some(1_000_000);
    assert_eq!(c.set_settings(&s).await.unwrap(), s);
    assert_eq!(c.status().await.unwrap().settings, s);

    c.set_priority(&[id], Priority::High).await.unwrap();
    assert_eq!(c.jobs().await.unwrap()[0].priority, Priority::High);
    assert!(!c.servers().await.unwrap().is_empty());
    c.remove(&[id]).await.unwrap();
    assert!(c.jobs().await.unwrap().is_empty());

    // Parar por la API.
    c.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), service.stop_requested().cancelled())
        .await
        .unwrap();
    service.stop().await;
    assert!(!paths.endpoint.exists());
    assert!(Client::connect(&paths).await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_token_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::for_db(dir.path().join("omniload.db"));
    let service = Service::start(&paths, fast_opts()).await.unwrap();
    let mut ep = Endpoint::read(&paths.endpoint).unwrap();
    ep.token = "0".repeat(64);
    let err = Client::new(&ep).jobs().await.unwrap_err();
    assert!(err.to_string().contains("token"), "{err}");
    service.stop().await;
}

#[test]
fn remote_queue_for_the_window() {
    // La ventana usa RemoteQueue desde su propio hilo, fuera del runtime.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let data = pseudo_random_bytes(256 * 1024, 32);
    let server = rt.block_on(TestServer::start(data.clone(), ServerConfig::default()));
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::for_db(dir.path().join("omniload.db"));
    let service = rt.block_on(Service::start(&paths, fast_opts())).unwrap();
    let client = rt.block_on(Client::connect(&paths)).unwrap();
    let q = RemoteQueue::new(rt.handle().clone(), client);
    assert!(q.online());

    q.update_settings(|s| s.max_active = 2);
    let mut new = NewJob::new(server.url("b.bin"), dir.path());
    new.package = "P".into();
    new.start = false;
    let id = q.add(new).unwrap();
    assert!(q.add(NewJob::new("nada", dir.path())).is_err());

    let wait = |pred: &dyn Fn(&RemoteQueue) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !pred(&q) {
            assert!(Instant::now() < deadline, "{:?}", q.snapshot());
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    wait(&|q| q.snapshot().len() == 1);
    assert_eq!(q.package_ids("P"), vec![id]);
    assert_eq!(q.snapshot()[0].state, State::Paused);
    q.start(&[id]);
    wait(&|q| q.snapshot().first().is_some_and(|j| j.state == State::Done));
    assert_eq!(std::fs::read(dir.path().join("b.bin")).unwrap(), data);
    assert_eq!(q.settings().max_active, 2);

    q.pause_all();
    assert!(q.is_held());
    wait(&|q| q.is_held() && q.online());
    q.remove(&[id]);
    assert!(q.snapshot().is_empty());

    rt.block_on(q.stop_service(Duration::from_secs(5)));
    let stop = service.stop_requested();
    rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), stop.cancelled())
            .await
            .unwrap();
        service.stop().await;
    });
}
