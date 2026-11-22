//! Lo poco de la interfaz que cambia según el sistema operativo.

use std::io;
use std::path::Path;
use std::process::Command;

/// Abre una página web en el navegador del sistema (solo `https://`).
pub fn open_url(url: &str) -> io::Result<()> {
    if !url.starts_with("https://") {
        return Err(io::Error::other("solo se abren enlaces https"));
    }
    open_with_system(Path::new(url))
}

/// Abre una carpeta en el explorador de archivos del sistema.
pub fn open_folder(path: &Path) -> io::Result<()> {
    open_with_system(path)
}

fn open_with_system(path: &Path) -> io::Result<()> {
    #[cfg(target_os = "windows")]
    let mut cmd = Command::new("explorer");
    #[cfg(target_os = "macos")]
    let mut cmd = Command::new("open");
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let mut cmd = Command::new("xdg-open");

    // No miramos el código de salida: el explorador de Windows, por ejemplo,
    // devuelve 1 aunque abra la carpeta bien. Lo esperamos en otro hilo para
    // no dejar procesos zombi en Unix.
    let mut child = cmd.arg(path).spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(())
}
