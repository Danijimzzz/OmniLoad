//! BitTorrent de punta a punta sin internet: un torrent creado aquí, una
//! sesión que lo siembra desde una carpeta y OmniLoad que lo descarga de
//! ella (el DHT y los rastreadores están apagados; el par se da a mano).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use librqbit::spawn_utils::BlockingSpawner;
use librqbit::{
    AddTorrent, AddTorrentOptions, CreateTorrentOptions, ListenerMode, ListenerOptions, Session,
    SessionOptions, create_torrent,
};
use omni_core::{Error, Network, Progress};
use omni_grab::Found;
use omni_hosts::{Hosts, Torrents};
use omni_testserver::pseudo_random_bytes;
use tokio_util::sync::CancellationToken;
use url::Url;

/// librqbit abre siempre un socket UDP IPv6 para los rastreadores: en un
/// equipo sin IPv6 (algunos contenedores) estas pruebas no pueden correr.
fn ipv6_available() -> bool {
    std::net::UdpSocket::bind("[::]:0").is_ok()
}

fn local_opts(addr: SocketAddr) -> SessionOptions {
    SessionOptions {
        dht: None,
        disable_trackers: true,
        ipv4_only: true,
        disable_local_service_discovery: true,
        listen: Some(ListenerOptions {
            mode: ListenerMode::TcpOnly,
            listen_addr: addr,
            ipv4_only: true,
            ..ListenerOptions::default()
        }),
        ..SessionOptions::default()
    }
}

/// Un torrent de `datos.bin` sembrado en `127.0.0.1:<puerto>`.
async fn seeded_torrent(
    seed_dir: &std::path::Path,
    torrent_file: &std::path::Path,
    data: &[u8],
) -> (Arc<Session>, SocketAddr) {
    let file = seed_dir.join("datos.bin");
    std::fs::write(&file, data).unwrap();
    let created = create_torrent(
        &file,
        CreateTorrentOptions {
            name: None,
            trackers: vec![],
            piece_length: Some(32 * 1024),
        },
        &BlockingSpawner::new(2),
    )
    .await
    .unwrap();
    let bytes = created.as_bytes().unwrap();
    std::fs::write(torrent_file, &bytes).unwrap();

    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let seeder = Session::new_with_opts(seed_dir.to_path_buf(), local_opts(addr))
        .await
        .unwrap();
    let handle = seeder
        .add_torrent(
            AddTorrent::from_bytes(bytes),
            Some(AddTorrentOptions {
                overwrite: true,
                output_folder: Some(seed_dir.to_string_lossy().into_owned()),
                ..AddTorrentOptions::default()
            }),
        )
        .await
        .unwrap()
        .into_handle()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), handle.wait_until_completed())
        .await
        .expect("el sembrador comprueba el archivo")
        .unwrap();
    (seeder, addr)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn downloads_a_torrent_file_from_a_peer() {
    if !ipv6_available() {
        eprintln!("sin IPv6: prueba omitida");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let seed_dir = tmp.path().join("siembra");
    std::fs::create_dir_all(&seed_dir).unwrap();
    let torrent = tmp.path().join("datos.torrent");
    let data = pseudo_random_bytes(600 * 1024, 5);
    let (_seeder, seed_addr) = seeded_torrent(&seed_dir, &torrent, &data).await;

    let hosts = Hosts::empty()
        .with(Torrents::local(([127, 0, 0, 1], 0).into()).with_peers(vec![seed_addr]));
    let client = Network::system_default().unwrap().client().clone();
    let url = Url::from_file_path(&torrent).unwrap();

    // El capturador ve nombre y tamaño.
    let Found::File(info) = hosts.checkers().check(&client, &url).await.unwrap() else {
        panic!("se esperaba un archivo")
    };
    assert_eq!(info.filename, "datos.bin");
    assert_eq!(info.size, Some(data.len() as u64));

    // Y la cola lo descarga.
    use omni_queue::Resolver;
    let cancel = CancellationToken::new();
    let link = hosts
        .resolver(client)
        .resolve(url.as_str(), &cancel)
        .await
        .unwrap();
    let out = tmp.path().join("descargas");
    let progress = Arc::new(Progress::default());
    let done = tokio::time::timeout(
        Duration::from_secs(60),
        link.custom.unwrap().run(&out, progress.clone(), cancel),
    )
    .await
    .expect("la descarga termina")
    .unwrap();
    assert_eq!(done.size, data.len() as u64);
    assert_eq!(std::fs::read(out.join("datos.bin")).unwrap(), data);
    assert_eq!(progress.downloaded(), data.len() as u64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_stops_the_torrent() {
    if !ipv6_available() {
        eprintln!("sin IPv6: prueba omitida");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let torrent = tmp.path().join("x.torrent");
    let seed_dir = tmp.path().join("siembra");
    std::fs::create_dir_all(&seed_dir).unwrap();
    let data = pseudo_random_bytes(64 * 1024, 6);
    let (_seeder, _addr) = seeded_torrent(&seed_dir, &torrent, &data).await;

    // Sin pares: nunca termina; cancelar devuelve la mano.
    let hosts = Hosts::empty().with(Torrents::local(([127, 0, 0, 1], 0).into()));
    let client = Network::system_default().unwrap().client().clone();
    let url = Url::from_file_path(&torrent).unwrap();
    use omni_queue::Resolver;
    let cancel = CancellationToken::new();
    let link = hosts
        .resolver(client)
        .resolve(url.as_str(), &cancel)
        .await
        .unwrap();
    let out = tmp.path().join("descargas");
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        c2.cancel();
    });
    let err = tokio::time::timeout(
        Duration::from_secs(30),
        link.custom
            .unwrap()
            .run(&out, Arc::new(Progress::default()), cancel),
    )
    .await
    .expect("cancelar no se cuelga")
    .unwrap_err();
    assert!(matches!(err, Error::Cancelled), "{err}");
}
