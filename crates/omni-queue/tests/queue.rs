//! Pruebas de la cola contra el servidor falso.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use omni_core::DownloadOptions;
use omni_queue::{
    JobInfo, NewJob, Priority, Queue, QueueConfig, ResolveFuture, ResolvedLink, Resolver, State,
};
use omni_store::Store;
use omni_testserver::{ServerConfig, TestServer, pseudo_random_bytes};
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;

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

fn open(store: Arc<Store>) -> Queue {
    let mut cfg = QueueConfig::new(store);
    cfg.base = fast_opts();
    Queue::open(Handle::current(), cfg).unwrap()
}

fn memory_queue() -> Queue {
    open(Arc::new(Store::open_in_memory().unwrap()))
}

fn new(url: String, dir: &Path, start: bool) -> NewJob {
    NewJob {
        package: "Pruebas".into(),
        start,
        ..NewJob::new(url, dir)
    }
}

fn row(q: &Queue, id: u64) -> JobInfo {
    q.snapshot().into_iter().find(|r| r.id == id).unwrap()
}

async fn wait_for(q: &Queue, id: u64, want: State) -> JobInfo {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let r = row(q, id);
        if r.state == want {
            return r;
        }
        assert!(
            Instant::now() < deadline,
            "la descarga no llegó a {want:?}: {r:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn slow_server_cfg(delay_ms: u64) -> ServerConfig {
    ServerConfig {
        chunk: 16 * 1024,
        chunk_delay: Some(Duration::from_millis(delay_ms)),
        ..ServerConfig::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_pause_resume_finish() {
    let data = pseudo_random_bytes(2 * 1024 * 1024, 7);
    let server = TestServer::start(data.clone(), slow_server_cfg(5)).await;
    let dir = tempfile::tempdir().unwrap();
    let q = memory_queue();

    let id = q
        .add(new(server.url("video.bin"), dir.path(), true))
        .unwrap();
    assert_eq!(q.snapshot()[0].name, "video.bin");
    let r = wait_for(&q, id, State::Running).await;
    assert!(r.fraction().is_some());

    while row(&q, id).fraction().unwrap_or(0.0) < 0.1 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    q.stop(&[id]);
    let paused = wait_for(&q, id, State::Paused).await;
    assert_eq!(paused.status(), "Pausada");
    assert!(!dir.path().join("video.bin").exists());

    // Añadir el mismo enlace otra vez lo reanuda en lugar de duplicarlo.
    server.set_chunk_delay(None);
    assert_eq!(
        q.add(new(server.url("video.bin"), dir.path(), true)),
        Ok(id)
    );
    assert_eq!(q.snapshot().len(), 1);
    let done = wait_for(&q, id, State::Done).await;
    assert_eq!(done.fraction(), Some(1.0));
    assert_eq!(std::fs::read(dir.path().join("video.bin")).unwrap(), data);
    assert!(
        q.add(new(server.url("video.bin"), dir.path(), true))
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_respects_limit_and_priority() {
    let server = TestServer::start(pseudo_random_bytes(1024 * 1024, 3), slow_server_cfg(20)).await;
    let dir = tempfile::tempdir().unwrap();
    let q = memory_queue();
    q.set_max_active(1);

    // Añadida parada: no arranca sola.
    let parked = q
        .add(new(server.url("parada.bin"), dir.path(), false))
        .unwrap();
    let a = q.add(new(server.url("a.bin"), dir.path(), true)).unwrap();
    let b = q.add(new(server.url("b.bin"), dir.path(), true)).unwrap();
    let mut urgent = new(server.url("urgente.bin"), dir.path(), true);
    urgent.priority = Priority::High;
    let c = q.add(urgent).unwrap();

    assert!(row(&q, a).state.is_active());
    assert_eq!(row(&q, b).state, State::Queued);
    assert_eq!(row(&q, c).state, State::Queued);
    assert_eq!(row(&q, parked).state, State::Paused);
    assert_eq!(row(&q, parked).status(), "Sin iniciar");

    // Al parar la primera, entra la de prioridad alta antes que b.
    q.stop(&[a]);
    wait_for(&q, a, State::Paused).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !row(&q, c).state.is_active() {
        assert!(Instant::now() < deadline, "{:?}", q.snapshot());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(row(&q, b).state, State::Queued);

    // Pausa global: lo que corre vuelve a la cola y no arranca nada.
    q.pause_all();
    wait_for(&q, c, State::Queued).await;
    assert!(!q.any_active());
    assert_eq!(row(&q, b).status(), "En pausa");
    q.stop_all();
    assert!(q.snapshot().iter().all(|r| r.state == State::Paused));
    q.remove(&[a, b, c, parked]);
    assert!(q.snapshot().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_url_is_rejected_without_row() {
    let q = memory_queue();
    let dir = Path::new(".");
    assert!(q.add(new("ftp://ejemplo.com/a".into(), dir, true)).is_err());
    assert!(q.add(new("   ".into(), dir, true)).is_err());
    assert!(q.snapshot().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_and_settings_survive_a_restart() {
    let data = pseudo_random_bytes(2 * 1024 * 1024, 11);
    let server = TestServer::start(data.clone(), slow_server_cfg(5)).await;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("omniload.db");
    let downloads = dir.path().join("descargas");

    let (running, parked) = {
        let q = open(Arc::new(Store::open(&db).unwrap()));
        q.set_max_active(2);
        q.set_speed_limit(Some(10 * 1024 * 1024));
        q.set_max_per_server(6);
        let parked = q
            .add(new(server.url("parada.bin"), &downloads, false))
            .unwrap();
        q.set_priority(&[parked], Priority::Low);
        let running = q
            .add(new(server.url("grande.bin"), &downloads, true))
            .unwrap();
        while row(&q, running).downloaded < 128 * 1024 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Cerrar el programa a mitad de descarga.
        q.shutdown(Duration::from_secs(5)).await;
        (running, parked)
    };

    // El estado de reanudar ya no ensucia la carpeta de descargas.
    assert!(omni_core::state_path_for(&downloads.join("grande.bin")).exists());
    assert!(!downloads.join("grande.bin.part.omni").exists());
    server.set_chunk_delay(None);
    let before = server.ranges().len();
    let q = open(Arc::new(Store::open(&db).unwrap()));
    let s = q.settings();
    assert_eq!(s.max_active, 2);
    assert_eq!(s.speed_limit, Some(10 * 1024 * 1024));
    assert_eq!(s.max_per_server, 6);
    let p = row(&q, parked);
    assert_eq!(p.state, State::Paused);
    assert_eq!(p.priority, Priority::Low);
    assert_eq!(p.package, "Pruebas");
    // Lo que estaba descargándose sigue solo y reanuda desde el .part.
    let done = wait_for(&q, running, State::Done).await;
    assert_eq!(done.total, Some(data.len() as u64));
    assert_eq!(std::fs::read(downloads.join("grande.bin")).unwrap(), data);
    // Tras el sondeo (0-0), ninguna petición vuelve a empezar desde el
    // principio: lo ya bajado no se repite.
    let after = &server.ranges()[before..];
    assert!(
        after.iter().all(|&(start, end)| start > 0 || end == 0),
        "volvió a pedir desde el principio: {after:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_speed_limit_applies_live() {
    let data = pseudo_random_bytes(1024 * 1024, 12);
    let server = TestServer::start(data.clone(), ServerConfig::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let q = memory_queue();
    q.set_speed_limit(Some(64 * 1024));
    let id = q.add(new(server.url("a.bin"), dir.path(), true)).unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let r = row(&q, id);
    assert!(r.state.is_active(), "{r:?}");
    assert!(r.downloaded < 512 * 1024, "{r:?}");
    q.set_speed_limit(None);
    wait_for(&q, id, State::Done).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_job_speed_limit() {
    let data = pseudo_random_bytes(1024 * 1024, 13);
    let server = TestServer::start(data.clone(), ServerConfig::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let q = memory_queue();
    let mut slow = new(server.url("lenta.bin"), dir.path(), true);
    slow.speed_limit = Some(64 * 1024);
    let slow = q.add(slow).unwrap();
    let fast = q
        .add(new(server.url("rapida.bin"), dir.path(), true))
        .unwrap();
    // La lenta no acapara las conexiones del servidor: la rápida acaba
    // enseguida aunque la lenta haya cogido todos los huecos primero.
    let started = Instant::now();
    wait_for(&q, fast, State::Done).await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    let r = row(&q, slow);
    assert!(r.state.is_active(), "{r:?}");
    assert_eq!(r.speed_limit, Some(64 * 1024));
    q.set_job_speed_limit(&[slow], None);
    wait_for(&q, slow, State::Done).await;
}

/// Resolutor de prueba: "falso://<nombre>" se descarga del servidor de
/// pruebas con otro nombre, como haría un plugin de host.
struct FakeHost(String);

impl Resolver for FakeHost {
    fn resolve<'a>(&'a self, url: &'a str, _c: &'a CancellationToken) -> ResolveFuture<'a> {
        Box::pin(async move {
            let name = url.trim_start_matches("https://host-falso.test/");
            Ok(ResolvedLink {
                url: format!("{}{name}", self.0),
                filename: Some(format!("resuelto-{name}")),
                ..ResolvedLink::default()
            })
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolver_turns_links_into_downloads() {
    let data = pseudo_random_bytes(300 * 1024, 14);
    let server = TestServer::start(data.clone(), ServerConfig::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = QueueConfig::new(Arc::new(Store::open_in_memory().unwrap()));
    cfg.base = fast_opts();
    cfg.resolver = Arc::new(FakeHost(server.url("")));
    let q = Queue::open(Handle::current(), cfg).unwrap();
    let id = q
        .add(new(
            "https://host-falso.test/a.bin".into(),
            dir.path(),
            true,
        ))
        .unwrap();
    let done = wait_for(&q, id, State::Done).await;
    assert_eq!(done.name, "resuelto-a.bin");
    // La lista guarda el enlace original, no el directo.
    assert_eq!(done.url, "https://host-falso.test/a.bin");
    assert_eq!(
        std::fs::read(dir.path().join("resuelto-a.bin")).unwrap(),
        data
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_with_files_deletes_file_state_and_empty_folder() {
    let data = pseudo_random_bytes(512 * 1024, 3);
    let server = TestServer::start(data, ServerConfig::default()).await;
    let root = tempfile::tempdir().unwrap();
    let folder = root.path().join("Mi paquete");
    std::fs::create_dir_all(&folder).unwrap();
    let q = memory_queue();

    // Terminada: se borra el archivo y la carpeta, que queda vacía.
    let id = q.add(new(server.url("a.bin"), &folder, true)).unwrap();
    wait_for(&q, id, State::Done).await;
    assert!(folder.join("a.bin").exists());
    let n = q
        .remove_with_files(&[id], Some(root.path().to_path_buf()))
        .await;
    assert_eq!(n, 1);
    assert!(q.snapshot().is_empty());
    assert!(!folder.join("a.bin").exists());
    assert!(!folder.exists(), "la carpeta vacía debe borrarse");
    assert!(root.path().exists(), "la carpeta de descargas se conserva");

    // Con otro archivo ajeno dentro, la carpeta se queda.
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("mio.txt"), b"mio").unwrap();
    let id = q.add(new(server.url("b.bin"), &folder, true)).unwrap();
    wait_for(&q, id, State::Done).await;
    q.remove_with_files(&[id], Some(root.path().to_path_buf()))
        .await;
    assert!(!folder.join("b.bin").exists());
    assert!(folder.join("mio.txt").exists());

    // Solo quitar de la lista no toca el disco.
    let id = q.add(new(server.url("c.bin"), &folder, true)).unwrap();
    wait_for(&q, id, State::Done).await;
    q.remove(&[id]);
    assert!(folder.join("c.bin").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn schedule_blocks_and_releases_the_queue() {
    let data = pseudo_random_bytes(8 * 1024 * 1024, 9);
    let server = TestServer::start(data, slow_server_cfg(40)).await;
    let dir = tempfile::tempdir().unwrap();
    let q = memory_queue();

    // Un horario sin ningún día permitido: nada arranca.
    q.update_settings(|s| {
        s.schedule = omni_queue::Schedule {
            enabled: true,
            days: 0,
            // Todo el día (inicio = fin) para que solo mande `days`.
            from_min: 0,
            to_min: 0,
        }
    });
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(q.blocked_by_schedule());
    let id = q.add(new(server.url("a.bin"), dir.path(), true)).unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let r = row(&q, id);
    assert_eq!(r.state, State::Queued);
    assert_eq!(r.status(), "En pausa");

    // Se permite todos los días: arranca solo.
    q.update_settings(|s| s.schedule.days = omni_queue::ALL_DAYS);
    wait_for(&q, id, State::Running).await;
    assert!(!q.blocked_by_schedule());

    // Vuelve a quedar fuera de horario con la descarga en marcha: se pausa
    // guardando el progreso y vuelve a la cola.
    while row(&q, id).downloaded < 128 * 1024 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    q.update_settings(|s| s.schedule.days = 0);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let r = row(&q, id);
        if r.state == State::Queued {
            break;
        }
        assert!(Instant::now() < deadline, "no se pausó: {r:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Sin horario, termina.
    server.set_chunk_delay(None);
    q.update_settings(|s| s.schedule.enabled = false);
    wait_for(&q, id, State::Done).await;
}

/// Un programa externo (como yt-dlp) que descarga por su cuenta: aquí, un
/// script de `sh` que cuenta el progreso y crea el archivo.
#[cfg(unix)]
struct Script(&'static str);

#[cfg(unix)]
impl Resolver for Script {
    fn resolve<'a>(&'a self, _url: &'a str, _c: &'a CancellationToken) -> ResolveFuture<'a> {
        Box::pin(async move {
            Ok(ResolvedLink {
                external: Some(omni_queue::ExternalJob {
                    program: "sh".into(),
                    args: vec!["-c".into(), self.0.into(), "sh".into(), "{dir}".into()],
                    label: "script".into(),
                }),
                ..ResolvedLink::default()
            })
        })
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_program_downloads_and_reports() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = QueueConfig::new(Arc::new(Store::open_in_memory().unwrap()));
    cfg.base = fast_opts();
    cfg.resolver = Arc::new(Script(
        r#"echo "OMNI-PROGRESS 5 10"; printf 0123456789 > "$1/video.mp4"; echo "OMNI-FILE $1/video.mp4""#,
    ));
    let q = Queue::open(Handle::current(), cfg).unwrap();
    let id = q
        .add(new("https://youtube.test/v".into(), dir.path(), true))
        .unwrap();
    let done = wait_for(&q, id, State::Done).await;
    assert_eq!(done.name, "video.mp4");
    assert_eq!(
        std::fs::read(dir.path().join("video.mp4")).unwrap().len(),
        10
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_program_failure_shows_its_reason() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = QueueConfig::new(Arc::new(Store::open_in_memory().unwrap()));
    cfg.base = fast_opts();
    cfg.resolver = Arc::new(Script(r#"echo "ERROR: Video unavailable" >&2; exit 1"#));
    let q = Queue::open(Handle::current(), cfg).unwrap();
    let id = q
        .add(new("https://youtube.test/v".into(), dir.path(), true))
        .unwrap();
    let failed = wait_for(&q, id, State::Failed).await;
    assert!(failed.detail.contains("Video unavailable"), "{failed:?}");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pausing_kills_the_external_program() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = QueueConfig::new(Arc::new(Store::open_in_memory().unwrap()));
    cfg.base = fast_opts();
    cfg.resolver = Arc::new(Script(r#"echo "OMNI-PROGRESS 1 100"; sleep 60"#));
    let q = Queue::open(Handle::current(), cfg).unwrap();
    let id = q
        .add(new("https://youtube.test/v".into(), dir.path(), true))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(row(&q, id).state.is_active());
    q.stop(&[id]);
    wait_for(&q, id, State::Paused).await;
}
