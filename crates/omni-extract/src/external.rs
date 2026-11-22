//! RAR (y lo que no sepamos abrir por dentro) con un programa instalado:
//! 7-Zip, WinRAR/UnRAR o `unrar`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::{Error, Result};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    SevenZip,
    UnRar,
}

/// Busca un programa capaz de abrir RAR.
pub fn find_tool() -> Option<PathBuf> {
    tool().map(|(p, _)| p)
}

fn tool() -> Option<(PathBuf, Kind)> {
    for (name, kind) in [
        ("7zz", Kind::SevenZip),
        ("7z", Kind::SevenZip),
        ("7za", Kind::SevenZip),
        ("unrar", Kind::UnRar),
        ("UnRAR", Kind::UnRar),
    ] {
        if let Some(p) = on_path(name) {
            return Some((p, kind));
        }
    }
    #[cfg(windows)]
    {
        let roots = ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"];
        for var in roots {
            let Some(root) = std::env::var_os(var) else {
                continue;
            };
            let root = PathBuf::from(root);
            for (rel, kind) in [
                ("7-Zip\\7z.exe", Kind::SevenZip),
                ("WinRAR\\UnRAR.exe", Kind::UnRar),
                ("Programs\\7-Zip\\7z.exe", Kind::SevenZip),
            ] {
                let p = root.join(rel);
                if p.is_file() {
                    return Some((p, kind));
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    for p in [
        "/opt/homebrew/bin/7zz",
        "/usr/local/bin/7zz",
        "/opt/homebrew/bin/unrar",
    ] {
        let p = PathBuf::from(p);
        if p.is_file() {
            let kind = if p.ends_with("unrar") {
                Kind::UnRar
            } else {
                Kind::SevenZip
            };
            return Some((p, kind));
        }
    }
    None
}

fn on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        for candidate in [name.to_owned(), format!("{name}.exe")] {
            let p = dir.join(candidate);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

pub(crate) fn extract(
    archive: &Path,
    dest: &Path,
    password: Option<&str>,
    what: &'static str,
) -> Result<()> {
    let (exe, kind) = tool().ok_or(Error::NoTool(what))?;
    let mut cmd = Command::new(exe);
    match kind {
        Kind::SevenZip => {
            let mut out = std::ffi::OsString::from("-o");
            out.push(dest);
            cmd.arg("x").arg("-y").arg("-bd").arg(out);
            // Sin contraseña, una falsa evita que se quede esperando una.
            cmd.arg(format!("-p{}", password.unwrap_or("")));
            cmd.arg(archive);
        }
        Kind::UnRar => {
            cmd.arg("x").arg("-y").arg("-o+").arg("-idq");
            cmd.arg(match password {
                Some(p) => format!("-p{p}"),
                None => "-p-".to_owned(),
            });
            cmd.arg(archive);
            let mut d = dest.as_os_str().to_owned();
            d.push(std::path::MAIN_SEPARATOR_STR);
            cmd.arg(d);
        }
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd.output()?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    let low = err.to_ascii_lowercase();
    if low.contains("wrong password")
        || low.contains("password")
        || low.contains("encrypted")
        || low.contains("checksum error")
    {
        return Err(Error::Password);
    }
    let tail: String = err.lines().rev().take(3).collect::<Vec<_>>().join(" ");
    Err(Error::Failed(format!(
        "no se pudo descomprimir: {}",
        tail.trim()
    )))
}
