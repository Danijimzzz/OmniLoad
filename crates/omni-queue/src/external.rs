//! Descargas hechas por un programa externo (yt-dlp).
//!
//! La cola lanza el programa, lee su salida estándar (ver
//! [`crate::ExternalJob`]), actualiza el progreso y, si la descarga se
//! cancela, lo mata. Reanudar es lanzarlo otra vez: yt-dlp sigue con su
//! `.part`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use omni_core::{DownloadOutcome, Error, Progress};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::resolver::ExternalJob;

/// Cuántas líneas de error se guardan para el mensaje final.
const KEEP_STDERR: usize = 6;

pub(crate) async fn run(
    job: ExternalJob,
    dir: &Path,
    progress: Arc<Progress>,
    cancel: CancellationToken,
) -> omni_core::Result<DownloadOutcome> {
    std::fs::create_dir_all(dir)?;
    let mut cmd = Command::new(&job.program);
    cmd.args(
        job.args
            .iter()
            .map(|a| a.replace("{dir}", &dir.to_string_lossy())),
    )
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: sin ventana de consola
    let mut child = cmd.spawn().map_err(|e| {
        Error::host(
            job.label.clone(),
            format!("no se pudo ejecutar {}: {e}", job.program.display()),
        )
    })?;
    let stdout = child.stdout.take().expect("stdout pedido");
    let stderr = child.stderr.take().expect("stderr pedido");

    let errors = tokio::spawn(async move {
        let mut last: Vec<String> = Vec::new();
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim().to_owned();
            if line.is_empty() {
                continue;
            }
            if last.len() == KEEP_STDERR {
                last.remove(0);
            }
            last.push(line);
        }
        last
    });

    let mut file: Option<PathBuf> = None;
    let mut lines = BufReader::new(stdout).lines();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                return Err(Error::Cancelled);
            }
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    if let Some(rest) = line.strip_prefix("OMNI-PROGRESS ") {
                        // Bajado, y luego uno o dos totales (exacto, estimado):
                        // vale el primero que sea mayor que cero.
                        let mut nums = rest.split_whitespace().map(|n| n.parse::<f64>().ok());
                        if let Some(Some(done)) = nums.next() {
                            let total = nums.flatten().find(|t| *t > 0.0);
                            progress.report(done as u64, total.map(|t| t as u64));
                        }
                    } else if let Some(path) = line.strip_prefix("OMNI-FILE ") {
                        file = Some(PathBuf::from(path.trim()));
                    }
                }
                _ => break,
            }
        }
    }
    let status = tokio::select! {
        _ = cancel.cancelled() => {
            let _ = child.kill().await;
            return Err(Error::Cancelled);
        }
        s = child.wait() => s?,
    };
    let stderr_lines = errors.await.unwrap_or_default();
    if !status.success() {
        let why = stderr_lines
            .iter()
            .rev()
            .find(|l| l.to_ascii_uppercase().contains("ERROR"))
            .or(stderr_lines.last())
            .map(|l| l.trim_start_matches("ERROR:").trim().to_owned())
            .unwrap_or_else(|| format!("terminó con {status}"));
        return Err(Error::host(job.label, why));
    }
    let path = file
        .ok_or_else(|| Error::host(job.label.clone(), "terminó sin decir qué archivo ha creado"))?;
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    progress.report(size, Some(size));
    Ok(DownloadOutcome {
        path,
        size,
        resumed_from: 0,
        segmented: true,
        sha256: None,
    })
}
