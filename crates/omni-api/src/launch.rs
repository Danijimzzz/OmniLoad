//! Encontrar el servicio, o lanzarlo si no está en marcha.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tracing::info;

use crate::client::Client;
use crate::paths::Paths;

/// El programa `omnid` junto al que está en marcha (así se instalan juntos
/// `omniload`, `omni` y `omnid`).
pub fn omnid_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let path = exe
        .parent()?
        .join(format!("omnid{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Lanza `omnid` en segundo plano, sin ventana y separado de la terminal (no
/// se para al cerrarla ni con Ctrl+C). Lo que escribe va a `omnid.log`.
pub fn spawn_omnid(exe: &Path, paths: &Paths) -> io::Result<()> {
    std::fs::create_dir_all(paths.dir())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.log)?;
    let mut cmd = Command::new(exe);
    cmd.env("OMNILOAD_DB", &paths.db)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    detach(&mut cmd);
    let mut child = cmd.spawn()?;
    info!("lanzado {} (pid {})", exe.display(), child.id());
    // Recoger su estado al terminar, para no dejar un proceso zombi en Unix.
    std::thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

#[cfg(unix)]
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // Grupo de procesos propio: Ctrl+C en la terminal no le llega.
    cmd.process_group(0);
}

/// Se conecta al servicio; si no hay ninguno y se sabe dónde está `omnid`, lo
/// lanza y espera (como mucho `timeout`) a que conteste.
pub async fn connect_or_launch(
    paths: &Paths,
    exe: Option<&Path>,
    timeout: Duration,
) -> Result<Client, String> {
    if let Some(c) = Client::connect(paths).await {
        return Ok(c);
    }
    let Some(exe) = exe else {
        return Err("el servicio de OmniLoad (omnid) no está en marcha".into());
    };
    spawn_omnid(exe, paths).map_err(|e| format!("no se pudo lanzar {}: {e}", exe.display()))?;
    let deadline = Instant::now() + timeout;
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Some(c) = Client::connect(paths).await {
            return Ok(c);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "omnid no contesta; mira {} para ver qué pasó",
                paths.log.display()
            ));
        }
    }
}
