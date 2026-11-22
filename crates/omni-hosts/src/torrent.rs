//! BitTorrent: enlaces `magnet:`, archivos `.torrent` (de una web o del
//! disco) con [librqbit](https://github.com/ikatson/rqbit) (Rust puro,
//! Apache-2.0).
//!
//! No es un host sino otro protocolo, así que no pasa por el motor HTTP: el
//! plugin devuelve un [`CustomDownload`] que la cola lanza como cualquier otra
//! descarga (misma lista, misma ventana, pausa y reanudación). Pausar quita el
//! torrent de la sesión sin borrar nada; reanudar lo añade otra vez y librqbit
//! comprueba lo que ya hay en disco y sigue.
//!
//! Hay una única sesión de librqbit (DHT, puerto de escucha) para todos los
//! torrents, que se crea al usarla por primera vez. Al terminar un torrent
//! no se siembra.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use librqbit::TorrentStatsState;
use librqbit::api::TorrentIdOrHash;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, ListenerMode, ListenerOptions, Session,
    SessionOptions,
};
use omni_core::{DownloadOutcome, Error, Progress, RemoteInfo};
use omni_grab::{CheckFuture, Found, is_magnet, is_torrent_file};
use omni_queue::{CustomDownload, ResolveFuture, ResolvedLink, RunFuture};
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::Host;

const NAME: &str = "Torrent";
/// Lo que se espera a que un magnet encuentre pares con los metadatos.
const METADATA_TIMEOUT: Duration = Duration::from_secs(60);
/// Un `.torrent` pesa poco: más de esto no es un torrent.
const MAX_TORRENT_FILE: u64 = 32 * 1024 * 1024;
const POLL: Duration = Duration::from_millis(500);

fn is_torrent_url(url: &Url) -> bool {
    is_magnet(url)
        || is_torrent_file(url)
        || (matches!(url.scheme(), "http" | "https")
            && url.path().to_ascii_lowercase().ends_with(".torrent"))
}

fn fail(e: impl std::fmt::Display) -> Error {
    Error::host(NAME, e.to_string())
}

/// El motor BitTorrent.
pub struct Torrents {
    session: Arc<OnceCell<Arc<Session>>>,
    local: Option<SocketAddr>,
    peers: Vec<SocketAddr>,
}

impl Torrents {
    pub fn new() -> Self {
        Self {
            session: Arc::default(),
            local: None,
            peers: Vec::new(),
        }
    }

    /// Sin DHT y escuchando solo en esta dirección (pruebas en local).
    pub fn local(listen: SocketAddr) -> Self {
        Self {
            session: Arc::default(),
            local: Some(listen),
            peers: Vec::new(),
        }
    }

    /// Pares con los que probar siempre además de los que den el DHT y los
    /// rastreadores (una red privada, o pruebas sin internet).
    pub fn with_peers(mut self, peers: Vec<SocketAddr>) -> Self {
        self.peers = peers;
        self
    }

    fn initial_peers(&self) -> Option<Vec<SocketAddr>> {
        (!self.peers.is_empty()).then(|| self.peers.clone())
    }

    fn session_options(&self, ipv4_only: bool) -> SessionOptions {
        let mut opts = SessionOptions {
            ipv4_only,
            ..SessionOptions::default()
        };
        if let Some(addr) = self.local {
            opts.dht = None;
            opts.disable_trackers = true;
            opts.disable_local_service_discovery = true;
            opts.listen = Some(ListenerOptions {
                mode: ListenerMode::TcpOnly,
                listen_addr: addr,
                ipv4_only,
                ..ListenerOptions::default()
            });
        } else {
            // Escuchar permite que los demás se conecten a ti (más
            // velocidad). Sin UPnP: no se toca el router.
            opts.listen = Some(ListenerOptions {
                mode: ListenerMode::TcpOnly,
                listen_addr: if ipv4_only {
                    ([0, 0, 0, 0], 0).into()
                } else {
                    (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
                },
                ipv4_only,
                ..ListenerOptions::default()
            });
        }
        opts
    }

    async fn session(&self) -> omni_core::Result<Arc<Session>> {
        self.session
            .get_or_try_init(|| async {
                // Con IPv6 si el equipo lo tiene; si no, solo IPv4.
                let first = self.local.is_none();
                if first
                    && let Ok(s) =
                        Session::new_with_opts(std::env::temp_dir(), self.session_options(false))
                            .await
                {
                    return Ok(s);
                }
                Session::new_with_opts(std::env::temp_dir(), self.session_options(true))
                    .await
                    .map_err(|e| fail(format!("no se pudo iniciar BitTorrent: {e:#}")))
            })
            .await
            .cloned()
    }

    /// El torrent que hay detrás del enlace: un magnet tal cual, o los bytes
    /// del `.torrent`.
    async fn source(client: &reqwest::Client, url: &Url) -> omni_core::Result<Source> {
        if is_magnet(url) {
            return Ok(Source::Magnet(url.to_string()));
        }
        if url.scheme() == "file" {
            let path = url
                .to_file_path()
                .map_err(|_| Error::InvalidUrl(url.to_string()))?;
            let len = std::fs::metadata(&path)
                .map_err(|e| fail(format!("no se pudo abrir {}: {e}", path.display())))?
                .len();
            if len > MAX_TORRENT_FILE {
                return Err(fail("el archivo es demasiado grande para ser un .torrent"));
            }
            return Ok(Source::File(std::fs::read(path)?));
        }
        let resp = client.get(url.clone()).send().await?;
        if !resp.status().is_success() {
            return Err(crate::status_error(resp.status()));
        }
        if resp.content_length().is_some_and(|l| l > MAX_TORRENT_FILE) {
            return Err(fail("el archivo es demasiado grande para ser un .torrent"));
        }
        Ok(Source::File(resp.bytes().await?.to_vec()))
    }
}

impl Torrents {
    /// Diagnóstico: añade el torrent y, durante `seconds`, cuenta lo que
    /// pasa (pares, velocidad, errores) con `say`. Baja datos de verdad (solo
    /// el primer archivo) a una carpeta temporal que se borra al terminar.
    pub async fn debug(
        &self,
        client: &reqwest::Client,
        url: &Url,
        seconds: u64,
        mut say: impl FnMut(String),
    ) -> omni_core::Result<()> {
        let source = Self::source(client, url).await?;
        match &source {
            Source::Magnet(_) => say("origen: magnet".into()),
            Source::File(b) => say(format!("origen: archivo .torrent de {} bytes", b.len())),
        }
        let session = self.session().await?;
        say("sesión de BitTorrent iniciada".into());
        let dir =
            std::env::temp_dir().join(format!("omniload-torrent-debug-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let added = session
            .add_torrent(
                source.add(),
                Some(AddTorrentOptions {
                    overwrite: true,
                    only_files: Some(vec![0]),
                    output_folder: Some(dir.to_string_lossy().into_owned()),
                    initial_peers: self.initial_peers(),
                    ..AddTorrentOptions::default()
                }),
            )
            .await
            .map_err(|e| fail(format!("{e:#}")))?;
        let (id, handle) = match added {
            AddTorrentResponse::Added(id, h) | AddTorrentResponse::AlreadyManaged(id, h) => (id, h),
            AddTorrentResponse::ListOnly(_) => return Err(fail("respuesta inesperada")),
        };
        say(format!(
            "torrent añadido: {}",
            handle.name().unwrap_or_else(|| "(sin nombre aún)".into())
        ));
        for t in 0..=(seconds / 2) {
            let st = handle.stats();
            let mut line = format!(
                "[{:>3}s] estado={} bajado={} de {} bytes",
                t * 2,
                st.state,
                st.progress_bytes,
                st.total_bytes
            );
            if let Some(live) = &st.live {
                let p = &live.snapshot.peer_stats;
                line.push_str(&format!(
                    " | velocidad={} | pares: en cola={} conectando={} vivos={} (semillas {}) vistos={} muertos={} descartados={}",
                    live.download_speed,
                    p.queued,
                    p.connecting,
                    p.live,
                    p.live_seeders,
                    p.seen,
                    p.dead,
                    p.not_needed
                ));
            }
            if let Some(e) = &st.error {
                line.push_str(&format!(" | ERROR: {e}"));
            }
            say(line);
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let _ = session.delete(TorrentIdOrHash::Id(id), true).await;
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}

impl Default for Torrents {
    fn default() -> Self {
        Self::new()
    }
}

/// Lo que se le da a librqbit.
#[derive(Clone)]
enum Source {
    Magnet(String),
    File(Vec<u8>),
}

impl Source {
    fn add(&self) -> AddTorrent<'_> {
        match self {
            Source::Magnet(m) => AddTorrent::from_url(m.as_str()),
            Source::File(b) => AddTorrent::from_bytes(b.clone()),
        }
    }
}

impl Host for Torrents {
    fn name(&self) -> &str {
        NAME
    }

    fn handles(&self, url: &Url) -> bool {
        is_torrent_url(url)
    }

    fn parallel(&self) -> usize {
        3
    }

    fn check<'a>(&'a self, client: &'a reqwest::Client, url: &'a Url) -> CheckFuture<'a> {
        Box::pin(async move {
            let source = Self::source(client, url).await?;
            let session = self.session().await?;
            let listing = tokio::time::timeout(
                METADATA_TIMEOUT,
                session.add_torrent(
                    source.add(),
                    Some(AddTorrentOptions {
                        list_only: true,
                        initial_peers: self.initial_peers(),
                        ..AddTorrentOptions::default()
                    }),
                ),
            )
            .await
            .map_err(|_| {
                fail("no se encontró a nadie que comparta este torrent (¿enlace muerto o sin pares?)")
            })?
            .map_err(|e| fail(format!("{e:#}")))?;
            let AddTorrentResponse::ListOnly(info) = listing else {
                return Err(fail("respuesta inesperada de BitTorrent"));
            };
            let size: u64 = info.info.iter_file_lengths().sum();
            let name = info
                .info
                .name()
                .map(|n| n.into_owned())
                .unwrap_or_else(|| "torrent".into());
            Ok(Found::File(RemoteInfo {
                url: url.clone(),
                size: Some(size),
                // Se descarga entero y se puede reanudar, pero no por el
                // motor de bloques HTTP.
                accepts_ranges: false,
                etag: None,
                last_modified: None,
                filename: omni_core::filename::sanitize(&name),
            }))
        })
    }

    fn resolve<'a>(&'a self, client: &'a reqwest::Client, url: &'a Url) -> ResolveFuture<'a> {
        Box::pin(async move {
            let source = Self::source(client, url).await?;
            let session = self.session().await?;
            Ok(ResolvedLink {
                custom: Some(Arc::new(TorrentJob {
                    session,
                    source,
                    peers: self.initial_peers(),
                })),
                ..ResolvedLink::default()
            })
        })
    }
}

struct TorrentJob {
    session: Arc<Session>,
    source: Source,
    peers: Option<Vec<SocketAddr>>,
}

impl std::fmt::Debug for TorrentJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TorrentJob")
    }
}

impl CustomDownload for TorrentJob {
    fn run<'a>(
        &'a self,
        dir: &'a Path,
        progress: Arc<Progress>,
        cancel: CancellationToken,
    ) -> RunFuture<'a> {
        Box::pin(async move {
            std::fs::create_dir_all(dir)?;
            let added = tokio::select! {
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                r = self.session.add_torrent(
                    self.source.add(),
                    Some(AddTorrentOptions {
                        // Sigue sobre lo que ya hay en disco (reanudar).
                        overwrite: true,
                        output_folder: Some(dir.to_string_lossy().into_owned()),
                        initial_peers: self.peers.clone(),
                        ..AddTorrentOptions::default()
                    }),
                ) => r.map_err(|e| fail(format!("{e:#}")))?,
            };
            let (id, handle) = match added {
                AddTorrentResponse::Added(id, h) | AddTorrentResponse::AlreadyManaged(id, h) => {
                    (id, h)
                }
                AddTorrentResponse::ListOnly(_) => return Err(fail("respuesta inesperada")),
            };
            let mut started = false;
            let mut base = 0u64;
            let outcome = loop {
                let stats = handle.stats();
                // Mientras se comprueba lo que ya hay en disco (y en el primer
                // instante en marcha), esos bytes no son velocidad de bajada.
                if started {
                    // Las piezas se dan por buenas de una en una (32 MiB o más):
                    // con solo eso la velocidad iría a saltos. Sumar lo ya
                    // recibido de las piezas a medias la suaviza.
                    let received = stats.live.as_ref().map_or(0, |l| l.snapshot.fetched_bytes);
                    let shown = stats
                        .progress_bytes
                        .max(base.saturating_add(received))
                        .min(stats.total_bytes);
                    progress.report(shown, Some(stats.total_bytes));
                } else {
                    progress.report_resumed(stats.progress_bytes, Some(stats.total_bytes));
                    base = stats.progress_bytes;
                    started = matches!(stats.state, TorrentStatsState::Live);
                }
                if let Some(err) = stats.error {
                    break Err(fail(err));
                }
                if stats.finished {
                    let name = handle.name().unwrap_or_else(|| "torrent".into());
                    break Ok(DownloadOutcome {
                        path: dir.join(omni_core::filename::sanitize(&name)),
                        size: stats.total_bytes,
                        resumed_from: 0,
                        segmented: true,
                        sha256: None,
                    });
                }
                tokio::select! {
                    _ = cancel.cancelled() => break Err(Error::Cancelled),
                    _ = tokio::time::sleep(POLL) => {}
                }
            };
            // Quitar de la sesión sin borrar archivos: ni se siembra ni se
            // queda cargado; reanudar lo vuelve a añadir.
            let _ = self.session.delete(TorrentIdOrHash::Id(id), false).await;
            outcome
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handles(s: &str) -> bool {
        is_torrent_url(&Url::parse(s).unwrap())
    }

    #[test]
    fn recognises_torrent_links() {
        assert!(handles(
            "magnet:?xt=urn:btih:cab507494d02ebb1178b38f2e9d7be299c86b862&dn=x"
        ));
        assert!(handles(
            "https://releases.ubuntu.com/24.04/ubuntu.iso.torrent"
        ));
        assert!(handles("file:///home/yo/peli.torrent"));
        assert!(!handles("https://example.com/a.zip"));
        assert!(!handles("magnet:?dn=sin-hash"));
        assert!(!handles("file:///etc/passwd"));
    }
}
