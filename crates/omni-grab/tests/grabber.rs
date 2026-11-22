//! El capturador contra servidores locales: paquetes, tandas, duplicados,
//! plugins de hosts, velocidad de comprobación y guardar/recuperar.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use omni_core::{Network, RemoteInfo};
use omni_grab::{
    Check, CheckFuture, Checkers, Dest, FileRule, FolderFile, Found, GrabCommand, GrabPackage,
    Grabber, HostChecker, Pick,
};
use omni_queue::Priority;
use omni_testserver::{ServerConfig, TestServer, pseudo_random_bytes};
use tokio::runtime::Handle;
use url::Url;

fn grabber(checkers: Checkers) -> Grabber {
    let net = Network::system_default().unwrap();
    Grabber::new(Handle::current(), net.client().clone(), checkers)
}

fn dest(base: &str) -> Dest {
    Dest {
        base: base.into(),
        per_package: true,
        single_file_too: true,
        rules: Vec::new(),
    }
}

/// Espera a que haya `n` archivos y ninguno se esté comprobando.
async fn wait_checked(g: &Grabber, n: usize) -> Vec<GrabPackage> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let pkgs = g.snapshot();
        let items: Vec<_> = pkgs.iter().flat_map(|p| &p.items).collect();
        if items.len() == n && items.iter().all(|i| i.check != Check::Checking) {
            return pkgs;
        }
        assert!(Instant::now() < deadline, "sin terminar: {pkgs:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[test]
fn package_folders() {
    let mut d = dest("/descargas");
    assert_eq!(d.folder("Viaje", 3), Path::new("/descargas/Viaje"));
    assert_eq!(d.folder("peli", 1), Path::new("/descargas/peli"));
    // Nombres que Windows no admite.
    assert_eq!(
        d.folder("Fotos: 2026?", 2),
        Path::new("/descargas/Fotos_ 2026_")
    );
    assert_eq!(d.folder("con", 2), Path::new("/descargas/_con"));
    d.single_file_too = false;
    assert_eq!(d.folder("peli", 1), Path::new("/descargas"));
    assert_eq!(d.folder("Viaje", 3), Path::new("/descargas/Viaje"));
    d.per_package = false;
    assert_eq!(d.folder("Viaje", 3), Path::new("/descargas"));
}

#[test]
fn package_names() {
    use omni_grab::package_name;
    assert_eq!(package_name("asturias-2026.part1.rar"), "asturias-2026");
    assert_eq!(package_name("asturias-2026.part12.rar"), "asturias-2026");
    assert_eq!(package_name("copia.7z.001"), "copia");
    assert_eq!(package_name("linux.tar.gz"), "linux");
    assert_eq!(
        package_name("ubuntu-24.04.3-desktop-amd64.iso"),
        "ubuntu-24.04.3-desktop-amd64"
    );
    assert_eq!(package_name("SHA256SUMS"), "SHA256SUMS");
    assert_eq!(package_name(".bashrc"), ".bashrc");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn groups_into_packages_and_picks_selected() {
    let server = TestServer::start(pseudo_random_bytes(5000, 1), ServerConfig::default()).await;
    let g = grabber(Checkers::new());

    let text = format!(
        "{} {} {}",
        server.url("fotos.part1.rar"),
        server.url("fotos.part2.rar"),
        server.url("pelicula.mkv")
    );
    assert_eq!(g.add_to_batch(&text, 0), Ok(3));
    let pkgs = wait_checked(&g, 3).await;
    assert_eq!(pkgs.len(), 2);
    assert_eq!(pkgs[0].name, "fotos");
    assert_eq!(pkgs[0].items.len(), 2);
    assert_eq!(
        pkgs[0].folder(&dest("/descargas")),
        Path::new("/descargas/fotos")
    );
    assert_eq!(pkgs[1].name, "pelicula");
    assert_eq!(pkgs[1].items[0].size, Some(5000));
    assert_eq!(pkgs[1].items[0].host, "127.0.0.1");
    assert_eq!(pkgs[1].items[0].check, Check::Online { resumable: true });

    // Repetir los mismos enlaces no los duplica.
    assert!(g.add_to_batch(&text, 0).is_err());
    assert!(g.add_to_batch("sin enlaces", 0).is_err());

    // Propiedades del paquete y selección, con las órdenes de la API.
    g.apply(GrabCommand::RenamePackage {
        id: pkgs[0].id,
        name: "Viaje".into(),
    });
    g.apply(GrabCommand::SetPackagePriority {
        id: pkgs[0].id,
        priority: Priority::High,
    });
    g.apply(GrabCommand::SelectItem {
        id: pkgs[0].items[1].id,
        selected: false,
    });
    g.apply(GrabCommand::SelectPackage {
        id: pkgs[1].id,
        selected: false,
    });
    let picked = g.take(Pick::Selected, &dest("/descargas"));
    assert_eq!(picked.len(), 1);
    assert_eq!(picked[0].name, "fotos.part1.rar");
    assert_eq!(picked[0].package, "Viaje");
    assert_eq!(picked[0].folder, Path::new("/descargas/Viaje"));
    assert_eq!(picked[0].priority, Priority::High);
    let job = picked[0].clone().into_job(false);
    assert_eq!(job.name.as_deref(), Some("fotos.part1.rar"));
    assert_eq!(job.size, Some(5000));
    assert!(!job.start);

    // Lo no marcado sigue en el capturador.
    let left = g.snapshot();
    assert_eq!(left.iter().map(|p| p.items.len()).sum::<usize>(), 2);
    g.apply(GrabCommand::Clear);
    assert!(g.snapshot().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batches_of_copied_links() {
    let server = TestServer::start(pseudo_random_bytes(3000, 2), ServerConfig::default()).await;
    let g = grabber(Checkers::new());

    g.add_to_batch(&server.url("antes.zip"), 0).unwrap();
    let text = format!(
        "{}\n{}\nhttp://127.0.0.1:1/caido.zip",
        server.url("a.zip"),
        server.url("b.zip")
    );
    let added = g.add_links(&text, true).unwrap();
    assert_eq!(added.added, 3);
    let batch = added.batch;
    assert_ne!(batch, 0);
    // Cada tanda nueva tiene su número.
    let other = g.add_links(&server.url("c.zip"), true).unwrap();
    assert_ne!(other.batch, batch);
    wait_checked(&g, 5).await;

    let items = g.batch_items(batch);
    assert_eq!(items.len(), 3);
    // El más reciente primero.
    assert_eq!(items[0].name, "caido.zip");
    assert!(matches!(items[0].check, Check::Offline(_)));
    assert!(!items[0].selected);

    // Solo se llevan los disponibles de la tanda.
    let picked = g.take(Pick::Batch(batch), &dest("/d"));
    let mut names: Vec<_> = picked.iter().map(|p| p.name.as_str()).collect();
    names.sort();
    assert_eq!(names, ["a.zip", "b.zip"]);
    assert_eq!(g.batch_items(batch).len(), 1);

    g.apply(GrabCommand::DiscardBatch { batch });
    assert!(g.batch_items(batch).is_empty());
    // Lo que no era de la tanda sigue ahí.
    assert_eq!(g.batch_items(0).len(), 1);
}

/// El plan pide comprobar 100 enlaces en menos de 10 s. Con un servidor que
/// tarda 100 ms en contestar cada petición, uno detrás de otro serían 10 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hundred_links_quickly() {
    let cfg = ServerConfig {
        response_delay: Some(Duration::from_millis(100)),
        ..ServerConfig::default()
    };
    let server = TestServer::start(pseudo_random_bytes(1000, 3), cfg).await;
    let g = grabber(Checkers::new());
    let text: Vec<String> = (0..100)
        .map(|i| server.url(&format!("archivo{i:03}.bin")))
        .collect();
    let t0 = Instant::now();
    assert_eq!(g.add_to_batch(&text.join("\n"), 0), Ok(100));
    let pkgs = wait_checked(&g, 100).await;
    let took = t0.elapsed();
    assert!(
        pkgs.iter()
            .flat_map(|p| &p.items)
            .all(|i| i.size == Some(1000)),
        "{pkgs:?}"
    );
    // Una petición por enlace, no dos.
    assert_eq!(server.requests(), 100);
    // Ocho a la vez contra el mismo servidor (no más, para no provocar
    // bloqueos): unos 1,3 s. Margen para máquinas lentas de CI.
    assert!(took < Duration::from_secs(5), "tardó {took:?}");
    assert!(
        took >= Duration::from_millis(1200),
        "¿más de 8 a la vez? {took:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn links_already_in_downloads_come_unselected() {
    let server = TestServer::start(pseudo_random_bytes(100, 4), ServerConfig::default()).await;
    let known = server.url("ya-esta.zip");
    let k = known.clone();
    let g = grabber(Checkers::new()).with_known_links(Arc::new(move |u| u == k));
    g.add_to_batch(&format!("{known} {}", server.url("nuevo.zip")), 0)
        .unwrap();
    let pkgs = wait_checked(&g, 2).await;
    let items: Vec<_> = pkgs.iter().flat_map(|p| &p.items).collect();
    let dup = items.iter().find(|i| i.name == "ya-esta.zip").unwrap();
    let new = items.iter().find(|i| i.name == "nuevo.zip").unwrap();
    assert!(dup.duplicate && !dup.selected);
    assert!(!new.duplicate && new.selected);
    // Se puede marcar a mano si de verdad se quiere otra vez.
    g.set_item_selected(dup.id, true);
    assert_eq!(g.take(Pick::Selected, &dest("/d")).len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recheck_after_failure() {
    // Las dos primeras peticiones fallan con 503.
    let cfg = ServerConfig {
        fail_first: 2,
        ..ServerConfig::default()
    };
    let server = TestServer::start(pseudo_random_bytes(10, 5), cfg).await;
    let g = grabber(Checkers::new());
    g.add_to_batch(&format!("{} {}", server.url("a"), server.url("b")), 0)
        .unwrap();
    let pkgs = wait_checked(&g, 2).await;
    assert!(
        pkgs.iter()
            .flat_map(|p| &p.items)
            .all(|i| matches!(i.check, Check::Offline(_)))
    );
    g.apply(GrabCommand::Recheck { ids: vec![] });
    let pkgs = wait_checked(&g, 2).await;
    assert!(
        pkgs.iter()
            .flat_map(|p| &p.items)
            .all(|i| matches!(i.check, Check::Online { .. })),
        "{pkgs:?}"
    );
}

/// Un plugin de mentira, como serán los de Mega o Gofile: un enlace de
/// carpeta da dos archivos ya comprobados y uno de archivo da su nombre.
struct FakeHost;

impl HostChecker for FakeHost {
    fn name(&self) -> &str {
        "Falso"
    }
    fn handles(&self, url: &Url) -> bool {
        url.host_str() == Some("falso.example")
    }
    fn check<'a>(&'a self, _client: &'a reqwest::Client, url: &'a Url) -> CheckFuture<'a> {
        Box::pin(async move {
            if url.path().starts_with("/carpeta") {
                Ok(Found::Folder {
                    name: Some("Vacaciones".into()),
                    files: (1..=2)
                        .map(|i| FolderFile {
                            url: Url::parse(&format!("https://falso.example/f/{i}#clave")).unwrap(),
                            name: format!("foto{i}.jpg"),
                            size: Some(i * 100),
                            resumable: true,
                        })
                        .collect(),
                })
            } else if url.path() == "/borrado" {
                Err(omni_core::Error::LinkExpired { status: 404 })
            } else {
                Ok(Found::File(RemoteInfo {
                    url: url.clone(),
                    size: Some(42),
                    accepts_ranges: false,
                    etag: None,
                    last_modified: None,
                    filename: "video.mp4".into(),
                }))
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_plugins_check_their_links() {
    let checkers = Checkers::new().with(FakeHost);
    assert_eq!(checkers.names(), ["Falso"]);
    let g = grabber(checkers);
    g.add_to_batch(
        "https://falso.example/carpeta/x https://falso.example/v/1 https://falso.example/borrado",
        0,
    )
    .unwrap();
    let pkgs = wait_checked(&g, 4).await;
    let folder = pkgs.iter().find(|p| p.name == "Vacaciones").unwrap();
    assert_eq!(folder.items.len(), 2);
    assert_eq!(folder.items[1].size, Some(200));
    // La clave de Mega va en el fragmento: no se pierde.
    assert_eq!(folder.items[0].url, "https://falso.example/f/1#clave");
    let all: Vec<_> = pkgs.iter().flat_map(|p| &p.items).collect();
    assert!(all.iter().all(|i| i.host == "Falso"));
    let video = all.iter().find(|i| i.name == "video.mp4").unwrap();
    assert_eq!(video.check, Check::Online { resumable: false });
    let gone = all.iter().find(|i| i.name == "borrado").unwrap();
    assert_eq!(gone.check, Check::Offline("no existe (404)".into()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn save_and_restore() {
    let server = TestServer::start(pseudo_random_bytes(77, 6), ServerConfig::default()).await;
    let g = grabber(Checkers::new());
    g.add_to_batch(
        &format!("{} {}", server.url("x.zip"), server.url("y.zip")),
        0,
    )
    .unwrap();
    let before = wait_checked(&g, 2).await;
    let gen0 = g.generation();
    g.rename_package(before[0].id, "Mío");
    assert!(g.generation() > gen0);

    let json = serde_json::to_string(&g.save()).unwrap();
    let g2 = grabber(Checkers::new());
    g2.restore(serde_json::from_str(&json).unwrap());
    let after = g2.snapshot();
    assert_eq!(after.len(), before.len());
    assert_eq!(after[0].name, "Mío");
    assert_eq!(after[0].items[0].size, Some(77));
    // Los ids nuevos no chocan con los recuperados.
    g2.add_to_batch(&server.url("z.zip"), 0).unwrap();
    let all = wait_checked(&g2, 3).await;
    let mut ids: Vec<u64> = all.iter().map(|p| p.id).collect();
    ids.extend(all.iter().flat_map(|p| p.items.iter().map(|i| i.id)));
    let n = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), n);
}

#[test]
fn file_rules_parse_and_match() {
    let r = FileRule::parse(" .MP4, mkv;*avi  ", "/videos").unwrap();
    assert_eq!(r.extensions, ["mp4", "mkv", "avi"]);
    assert!(r.matches("peli.MKV"));
    assert!(r.matches("a.b.mp4"));
    assert!(!r.matches("peli.mkv.part"));
    assert!(!r.matches("mp4"));
    assert!(FileRule::parse("", "/x").is_none());
    assert!(FileRule::parse("zip", "").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rules_send_files_to_their_folder_unless_the_package_has_one() {
    let server = TestServer::start(pseudo_random_bytes(5000, 2), ServerConfig::default()).await;
    let g = grabber(Checkers::new());
    let text = format!(
        "{} {} {}",
        server.url("a.mp4"),
        server.url("b.zip"),
        server.url("c.mp4")
    );
    g.add_to_batch(&text, 0).unwrap();
    wait_checked(&g, 3).await;
    let mut d = dest("/descargas");
    d.rules = vec![
        FileRule::parse("mp4", "/videos").unwrap(),
        FileRule::parse("mp4 zip", "/otra").unwrap(), // la primera que coincida manda
    ];
    let mut picked = g.take(Pick::Selected, &d);
    picked.sort_by(|a, b| a.name.cmp(&b.name));
    let folders: Vec<_> = picked
        .iter()
        .map(|p| (p.name.as_str(), &p.folder))
        .collect();
    assert_eq!(folders.len(), 3);
    assert_eq!(picked[0].folder, Path::new("/videos"));
    assert_eq!(picked[1].folder, Path::new("/otra"));
    assert_eq!(picked[2].folder, Path::new("/videos"));

    // Con carpeta elegida a mano en el paquete, las reglas no la cambian.
    let g = grabber(Checkers::new());
    g.add_to_batch(&server.url("d.mp4"), 0).unwrap();
    let pkgs = wait_checked(&g, 1).await;
    g.apply(GrabCommand::SetPackageFolder {
        id: pkgs[0].id,
        folder: "/mia".into(),
    });
    let picked = g.take(Pick::Selected, &d);
    assert_eq!(picked[0].folder, Path::new("/mia"));
}
