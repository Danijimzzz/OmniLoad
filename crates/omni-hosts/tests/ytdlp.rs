//! yt-dlp con un programa de mentira (un script de `sh`) que imita lo que
//! hace el de verdad: `-J` da el JSON y, sin `-J`, descarga.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use omni_core::Network;
use omni_grab::Found;
use omni_hosts::{Hosts, YtDlp};
use omni_queue::{NewJob, Queue, QueueConfig, State};
use omni_store::Store;
use tokio::runtime::Handle;
use url::Url;

fn fake_ytdlp(dir: &Path) -> PathBuf {
    let path = dir.join("yt-dlp");
    std::fs::write(
        &path,
        r#"#!/bin/sh
case "$*" in
  *--flat-playlist*)
    echo '{"_type":"playlist","title":"Mi lista","entries":[{"title":"Uno","webpage_url":"https://www.youtube.com/watch?v=a"},{"title":"Dos","url":"https://www.youtube.com/watch?v=b"},{"title":"sin enlace","url":"b"}]}'
    exit 0;;
  *"-J"*)
    case "$*" in
      *roto*) echo "ERROR: [youtube] roto: Video unavailable" >&2; exit 1;;
    esac
    echo '{"title":"Mi vídeo: la película","ext":"webm","filesize_approx":5000}'
    exit 0;;
esac
# descarga: -P <carpeta>
while [ $# -gt 0 ]; do
  if [ "$1" = "-P" ]; then dir="$2"; fi
  shift
done
echo "OMNI-PROGRESS 4 8 0"
printf 12345678 > "$dir/Mi video.mp4"
echo "OMNI-FILE $dir/Mi video.mp4"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn client() -> reqwest::Client {
    Network::system_default().unwrap().client().clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_resolve_and_download() {
    let tmp = tempfile::tempdir().unwrap();
    let exe = fake_ytdlp(tmp.path());
    let hosts = Hosts::empty().with(YtDlp::with_exe(&exe, None));
    let checkers = hosts.checkers();

    let url = Url::parse("https://www.youtube.com/watch?v=abc").unwrap();
    let Found::File(info) = checkers.check(&client(), &url).await.unwrap() else {
        panic!("se esperaba un vídeo")
    };
    assert_eq!(info.size, Some(5000));
    assert!(!info.accepts_ranges);
    assert!(info.filename.ends_with(".webm"), "{}", info.filename);

    let list = Url::parse("https://www.youtube.com/playlist?list=PL1").unwrap();
    let Found::Folder { name, files } = checkers.check(&client(), &list).await.unwrap() else {
        panic!("se esperaba una lista")
    };
    assert_eq!(name.as_deref(), Some("Mi lista"));
    assert_eq!(files.len(), 2);
    assert_eq!(files[1].name, "Dos");

    let broken = Url::parse("https://www.youtube.com/watch?v=roto").unwrap();
    let err = checkers.check(&client(), &broken).await.unwrap_err();
    assert!(err.to_string().contains("Video unavailable"), "{err}");

    // Descargar de verdad con la cola.
    let out = tmp.path().join("descargas");
    let mut cfg = QueueConfig::new(Arc::new(Store::open_in_memory().unwrap()));
    cfg.resolver = Arc::new(hosts.resolver(client()));
    let q = Queue::open(Handle::current(), cfg).unwrap();
    let id = q
        .add(NewJob {
            package: "Vídeos".into(),
            start: true,
            ..NewJob::new(url.to_string(), &out)
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let row = q.snapshot().into_iter().find(|r| r.id == id).unwrap();
        if row.state == State::Done {
            assert_eq!(row.name, "Mi video.mp4");
            break;
        }
        assert!(Instant::now() < deadline, "{row:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        std::fs::read(out.join("Mi video.mp4")).unwrap(),
        b"12345678"
    );
}

#[tokio::test]
async fn missing_program_says_how_to_install() {
    let hosts = Hosts::empty().with(YtDlp::with_exe("/no/existe/yt-dlp", None));
    let url = Url::parse("https://youtu.be/x").unwrap();
    let err = hosts.checkers().check(&client(), &url).await.unwrap_err();
    assert!(err.to_string().contains("no se pudo ejecutar"), "{err}");
}
