//! Vídeo y audio con yt-dlp (YouTube, Vimeo, Twitch, TikTok, X, Reddit,
//! SoundCloud, Bandcamp…).
//!
//! yt-dlp se ejecuta como programa externo (es de dominio público y se
//! actualiza solo con `yt-dlp -U`):
//!
//! - **Comprobar:** `yt-dlp -J` da título, duración y tamaño; en una lista de
//!   reproducción, los vídeos que contiene.
//! - **Descargar:** lo hace yt-dlp entero (une audio y vídeo con ffmpeg si lo
//!   encuentra) y la cola le sigue el progreso por su salida. Reanudar es
//!   volver a lanzarlo: sigue con su archivo `.part`.
//!
//! Se encuentra en: la variable `OMNILOAD_YTDLP`, la carpeta de `omniload` y
//! el `PATH`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use omni_core::{Error, RemoteInfo};
use omni_grab::{CheckFuture, FolderFile, Found};
use omni_queue::{ExternalJob, ResolveFuture, ResolvedLink};
use tokio::process::Command;
use url::Url;

use crate::{Host, on_domain, segments};

const NAME: &str = "yt-dlp";
/// Tiempo máximo para que yt-dlp mire un enlace.
const CHECK_TIMEOUT: Duration = Duration::from_secs(90);
/// Plantilla de progreso (ver `ExternalJob`).
const PROGRESS_TEMPLATE: &str = "download:OMNI-PROGRESS %(progress.downloaded_bytes|0)s %(progress.total_bytes|0)s %(progress.total_bytes_estimate|0)s";

const NOT_INSTALLED: &str = "yt-dlp no está instalado. En Windows: abre PowerShell y escribe «winget install yt-dlp.yt-dlp» (o deja yt-dlp.exe junto a omniload.exe) y reinicia OmniLoad";

/// Las páginas de vídeo que se mandan a yt-dlp. Las demás siguen su camino
/// normal (enlace directo o página con enlaces).
fn is_video_page(url: &Url) -> bool {
    let seg = segments(url);
    let first = seg.first().map(String::as_str).unwrap_or("");
    if on_domain(url, "youtu.be") {
        return !seg.is_empty();
    }
    if on_domain(url, "youtube.com") || on_domain(url, "youtube-nocookie.com") {
        return matches!(
            first,
            "watch" | "shorts" | "live" | "embed" | "playlist" | "v"
        ) || first.starts_with('@')
            && seg.get(1).is_some_and(|s| s == "videos" || s == "shorts");
    }
    if on_domain(url, "vimeo.com") {
        return seg
            .first()
            .is_some_and(|s| s.chars().all(|c| c.is_ascii_digit()))
            || matches!(first, "channels" | "groups" | "album");
    }
    if on_domain(url, "twitch.tv") {
        return matches!(first, "videos" | "clip") || seg.get(1).is_some_and(|s| s == "clip");
    }
    if on_domain(url, "clips.twitch.tv") {
        return !seg.is_empty();
    }
    if on_domain(url, "tiktok.com") {
        return seg.iter().any(|s| s == "video") || on_domain(url, "vm.tiktok.com");
    }
    if on_domain(url, "twitter.com") || on_domain(url, "x.com") {
        return seg.get(1).is_some_and(|s| s == "status");
    }
    if on_domain(url, "reddit.com") {
        return seg.iter().any(|s| s == "comments");
    }
    if on_domain(url, "v.redd.it") {
        return !seg.is_empty();
    }
    if on_domain(url, "soundcloud.com") {
        return seg.len() >= 2;
    }
    if on_domain(url, "bandcamp.com") {
        return matches!(first, "track" | "album");
    }
    if on_domain(url, "dailymotion.com") {
        return first == "video";
    }
    if on_domain(url, "facebook.com") || on_domain(url, "fb.watch") {
        return seg
            .iter()
            .any(|s| s == "videos" || s == "watch" || s == "reel")
            || on_domain(url, "fb.watch");
    }
    if on_domain(url, "instagram.com") {
        return matches!(first, "reel" | "reels" | "p" | "tv");
    }
    false
}

/// ¿Es una lista de reproducción de YouTube?
fn is_playlist(url: &Url) -> bool {
    on_domain(url, "youtube.com")
        && url.path() == "/playlist"
        && url.query_pairs().any(|(k, _)| k == "list")
}

fn exe_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_owned()
    }
}

/// Busca un programa: variable de entorno, junto a OmniLoad y en el `PATH`.
fn locate(program: &str, env_var: &str) -> Option<PathBuf> {
    let name = exe_name(program);
    if let Some(p) = std::env::var_os(env_var).map(PathBuf::from)
        && p.is_file()
    {
        return Some(p);
    }
    let beside = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join(&name)));
    if let Some(p) = beside.filter(|p| p.is_file()) {
        return Some(p);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(&name))
            .find(|p| p.is_file())
    })
}

/// yt-dlp.
pub struct YtDlp {
    exe: Option<PathBuf>,
    ffmpeg: Option<PathBuf>,
}

impl YtDlp {
    /// Busca yt-dlp y ffmpeg en el equipo.
    pub fn new() -> Self {
        Self {
            exe: locate("yt-dlp", "OMNILOAD_YTDLP"),
            ffmpeg: locate("ffmpeg", "OMNILOAD_FFMPEG"),
        }
    }

    /// Con un yt-dlp concreto (pruebas).
    pub fn with_exe(exe: impl Into<PathBuf>, ffmpeg: Option<PathBuf>) -> Self {
        Self {
            exe: Some(exe.into()),
            ffmpeg,
        }
    }

    fn exe(&self) -> omni_core::Result<&Path> {
        self.exe
            .as_deref()
            .ok_or_else(|| Error::host(NAME, NOT_INSTALLED))
    }

    /// Los argumentos de una descarga.
    pub(crate) fn download_args(&self, url: &Url) -> Vec<String> {
        let mut a: Vec<String> = [
            "--no-playlist",
            "--no-warnings",
            "--no-colors",
            "--newline",
            "--no-simulate",
            "--windows-filenames",
            "--no-mtime",
            "-P",
            "{dir}",
            "-o",
            "%(title).150B.%(ext)s",
            "--progress-template",
            PROGRESS_TEMPLATE,
            "--print",
            "after_move:OMNI-FILE %(filepath)s",
        ]
        .map(String::from)
        .into();
        if let Some(ff) = &self.ffmpeg {
            // Con ffmpeg: la mejor imagen y el mejor sonido, unidos en MP4.
            a.extend(["-f", "bv*+ba/b", "--merge-output-format", "mp4"].map(String::from));
            if let Some(dir) = ff.parent() {
                a.extend([
                    "--ffmpeg-location".into(),
                    dir.to_string_lossy().into_owned(),
                ]);
            }
        } else {
            // Sin ffmpeg no se pueden unir: la mejor que ya venga unida.
            a.extend(["-f", "b"].map(String::from));
        }
        a.extend(["--".into(), url.to_string()]);
        a
    }

    async fn dump(&self, url: &Url) -> omni_core::Result<serde_json::Value> {
        let exe = self.exe()?;
        let mut cmd = Command::new(exe);
        cmd.args(["-J", "--no-warnings", "--no-colors"]);
        if is_playlist(url) {
            cmd.args(["--yes-playlist", "--flat-playlist"]);
        } else {
            cmd.arg("--no-playlist");
        }
        cmd.arg("--").arg(url.as_str());
        cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000);
        let out = tokio::time::timeout(CHECK_TIMEOUT, cmd.output())
            .await
            .map_err(|_| Error::host(NAME, "tardó demasiado en mirar el enlace"))?
            .map_err(|e| Error::host(NAME, format!("no se pudo ejecutar yt-dlp: {e}")))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(Error::host(NAME, explain(&err)));
        }
        serde_json::from_slice(&out.stdout)
            .map_err(|_| Error::host(NAME, "respuesta de yt-dlp ilegible"))
    }
}

/// Lo que dice yt-dlp, en una línea.
fn explain(stderr: &str) -> String {
    let line = stderr
        .lines()
        .rev()
        .find(|l| l.contains("ERROR"))
        .or_else(|| stderr.lines().last())
        .unwrap_or("error desconocido");
    let line = line.trim().trim_start_matches("ERROR:").trim();
    if line.contains("Sign in") || line.contains("confirm you") {
        format!("{line} (hace falta iniciar sesión: aún no se admiten cookies)")
    } else {
        line.to_owned()
    }
}

impl Default for YtDlp {
    fn default() -> Self {
        Self::new()
    }
}

impl Host for YtDlp {
    fn name(&self) -> &str {
        NAME
    }

    fn handles(&self, url: &Url) -> bool {
        is_video_page(url)
    }

    fn parallel(&self) -> usize {
        2
    }

    fn check<'a>(&'a self, _client: &'a reqwest::Client, url: &'a Url) -> CheckFuture<'a> {
        Box::pin(async move {
            let json = self.dump(url).await?;
            if json["_type"] == "playlist" {
                let files: Vec<FolderFile> = json["entries"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|e| {
                        let link = e["webpage_url"]
                            .as_str()
                            .or_else(|| e["url"].as_str().filter(|u| u.starts_with("http")))?;
                        Some(FolderFile {
                            url: Url::parse(link).ok()?,
                            name: e["title"].as_str().unwrap_or("vídeo").to_owned(),
                            size: None,
                            resumable: false,
                        })
                    })
                    .collect();
                if files.is_empty() {
                    return Err(Error::host(NAME, "la lista no tiene vídeos"));
                }
                return Ok(Found::Folder {
                    name: json["title"].as_str().map(str::to_owned),
                    files,
                });
            }
            let title = json["title"].as_str().unwrap_or("vídeo");
            let ext = json["ext"].as_str().unwrap_or("mp4");
            Ok(Found::File(RemoteInfo {
                url: url.clone(),
                size: crate::json_size(&json["filesize"])
                    .or_else(|| crate::json_size(&json["filesize_approx"])),
                accepts_ranges: false,
                etag: None,
                last_modified: None,
                filename: omni_core::filename::sanitize(&format!("{title}.{ext}")),
            }))
        })
    }

    fn resolve<'a>(&'a self, _client: &'a reqwest::Client, url: &'a Url) -> ResolveFuture<'a> {
        Box::pin(async move {
            let exe = self.exe()?.to_path_buf();
            if is_playlist(url) {
                return Err(Error::host(
                    NAME,
                    "es una lista de reproducción: añade sus vídeos uno a uno",
                ));
            }
            Ok(ResolvedLink {
                external: Some(ExternalJob {
                    program: exe,
                    args: self.download_args(url),
                    label: NAME.into(),
                }),
                ..ResolvedLink::default()
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(s: &str) -> bool {
        is_video_page(&Url::parse(s).unwrap())
    }

    #[test]
    fn recognises_video_pages() {
        for s in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ",
            "https://www.youtube.com/shorts/abc",
            "https://www.youtube.com/playlist?list=PL1",
            "https://vimeo.com/76979871",
            "https://www.twitch.tv/videos/123",
            "https://x.com/user/status/1",
            "https://www.reddit.com/r/videos/comments/abc/titulo/",
            "https://soundcloud.com/artista/cancion",
            "https://artista.bandcamp.com/track/cancion",
            "https://www.tiktok.com/@u/video/1",
        ] {
            assert!(video(s), "{s}");
        }
        for s in [
            "https://www.youtube.com/",
            "https://www.youtube.com/feed/subscriptions",
            "https://vimeo.com/",
            "https://example.com/watch?v=1",
            "https://x.com/user",
            "https://soundcloud.com/",
        ] {
            assert!(!video(s), "{s}");
        }
    }

    #[test]
    fn download_args_depend_on_ffmpeg() {
        let url = Url::parse("https://youtu.be/x").unwrap();
        let with = YtDlp::with_exe("/y/yt-dlp", Some("/f/ffmpeg".into())).download_args(&url);
        assert!(with.windows(2).any(|w| w == ["-f", "bv*+ba/b"]));
        assert!(with.windows(2).any(|w| w == ["--ffmpeg-location", "/f"]));
        assert_eq!(with.last().unwrap(), "https://youtu.be/x");
        let without = YtDlp::with_exe("/y/yt-dlp", None).download_args(&url);
        assert!(without.windows(2).any(|w| w == ["-f", "b"]));
        assert!(!without.iter().any(|a| a == "--ffmpeg-location"));
    }

    #[test]
    fn explains_errors() {
        assert_eq!(
            explain("WARNING: x\nERROR: [youtube] abc: Video unavailable\n"),
            "[youtube] abc: Video unavailable"
        );
    }
}
