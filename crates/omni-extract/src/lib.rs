//! Descomprime lo que se ha descargado: ZIP y 7z por dentro, RAR (y cualquier
//! otro formato) con 7-Zip, WinRAR o `unrar` si están instalados.
//!
//! - [`entry_points`] dice qué archivos de una lista son archivos comprimidos
//!   que hay que abrir (de un RAR en partes, solo la primera).
//! - [`extract`] abre uno, probando sin contraseña y luego con cada
//!   contraseña de la lista, y lo deja en una carpeta con su nombre.
//!
//! Los nombres de archivo de dentro nunca pueden salirse de la carpeta de
//! destino (`../`, rutas absolutas).

mod external;
mod sevenz;
mod zipfmt;

use std::path::{Component, Path, PathBuf};

pub use external::find_tool;

/// Formato de un archivo comprimido, por su nombre.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Zip,
    SevenZip,
    Rar,
}

/// Un archivo comprimido listo para abrir.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Archive {
    pub path: PathBuf,
    pub format: Format,
    /// Nombre sin la extensión ni el «.part1» (para la carpeta de salida).
    pub stem: String,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Hay contraseña y ninguna de las que se probaron vale.
    #[error("necesita una contraseña que no está en la lista")]
    Password,
    /// RAR u otro formato sin programa instalado para abrirlo.
    #[error("para abrir archivos {0} instala 7-Zip, WinRAR o unrar")]
    NoTool(&'static str),
    #[error("{0}")]
    Failed(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Lo que se hizo con un archivo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extracted {
    pub archive: PathBuf,
    /// Carpeta donde quedó el contenido.
    pub dest: PathBuf,
    /// Contraseña que valió, si hizo falta.
    pub password_used: bool,
}

/// Mira el nombre y dice si es un archivo comprimido que se abre por sí
/// solo, y no una parte intermedia de un volumen.
pub fn classify(path: &Path) -> Option<Archive> {
    let name = path.file_name()?.to_str()?;
    let lower = name.to_ascii_lowercase();
    let (format, stem_len) = entry_kind(&lower)?;
    Some(Archive {
        path: path.to_path_buf(),
        format,
        stem: name[..stem_len].trim_end_matches('.').to_owned(),
    })
}

/// Formato y longitud del nombre sin extensión, si es un punto de entrada.
fn entry_kind(lower: &str) -> Option<(Format, usize)> {
    if let Some(s) = lower.strip_suffix(".zip") {
        return Some((Format::Zip, s.len()));
    }
    if let Some(s) = lower.strip_suffix(".7z") {
        return Some((Format::SevenZip, s.len()));
    }
    if let Some(s) = lower.strip_suffix(".7z.001") {
        return Some((Format::SevenZip, s.len()));
    }
    let s = lower.strip_suffix(".rar")?;
    // `nombre.part1.rar` / `nombre.part01.rar`: solo la primera abre el
    // conjunto; las demás partes no son puntos de entrada.
    match volume_part(s) {
        Some((stem_len, 1)) => Some((Format::Rar, stem_len)),
        Some(_) => None,
        None => Some((Format::Rar, s.len())),
    }
}

/// `«algo.part03»` -> (longitud de «algo», 3).
fn volume_part(s: &str) -> Option<(usize, u32)> {
    let idx = s.rfind(".part")?;
    let digits = &s[idx + 5..];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((idx, digits.parse().ok()?))
}

/// De una lista de archivos, los comprimidos que hay que abrir.
pub fn entry_points<'a>(files: impl IntoIterator<Item = &'a Path>) -> Vec<Archive> {
    let mut out: Vec<Archive> = files.into_iter().filter_map(classify).collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Archivos que forman el conjunto (todas las partes de un RAR o 7z en
/// volúmenes), para borrarlos tras descomprimir.
pub fn parts_of(archive: &Archive) -> Vec<PathBuf> {
    let dir = archive.path.parent().unwrap_or_else(|| Path::new("."));
    let mut parts = vec![archive.path.clone()];
    let Ok(rd) = std::fs::read_dir(dir) else {
        return parts;
    };
    let stem = archive.stem.to_ascii_lowercase();
    for e in rd.flatten() {
        let p = e.path();
        if p == archive.path {
            continue;
        }
        let Some(n) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let n = n.to_ascii_lowercase();
        let Some(rest) = n.strip_prefix(&stem) else {
            continue;
        };
        let rest = rest.trim_start_matches('.');
        let part = match archive.format {
            Format::Rar => {
                (rest.starts_with("part")
                    && rest.ends_with(".rar")
                    && rest[4..rest.len() - 4].bytes().all(|b| b.is_ascii_digit()))
                    || (rest.len() == 3
                        && rest.starts_with('r')
                        && rest[1..].bytes().all(|b| b.is_ascii_digit()))
            }
            Format::SevenZip => {
                rest.starts_with("7z.") && rest[3..].bytes().all(|b| b.is_ascii_digit())
            }
            Format::Zip => false,
        };
        if part {
            parts.push(p);
        }
    }
    parts.sort();
    parts
}

/// Abre un archivo comprimido en `<carpeta del archivo>/<stem>/`. Prueba sin
/// contraseña y luego con cada una de `passwords`.
pub fn extract(archive: &Archive, passwords: &[String]) -> Result<Extracted> {
    let parent = archive.path.parent().unwrap_or_else(|| Path::new("."));
    let dest = unique_dir(parent, &archive.stem);
    let mut candidates: Vec<Option<&str>> = vec![None];
    candidates.extend(passwords.iter().map(|p| Some(p.as_str())));
    let mut last = Error::Password;
    for pw in candidates {
        std::fs::create_dir_all(&dest)?;
        let r = match archive.format {
            Format::Zip => zipfmt::extract(&archive.path, &dest, pw),
            Format::SevenZip => sevenz::extract(&archive.path, &dest, pw),
            Format::Rar => external::extract(&archive.path, &dest, pw, "RAR"),
        };
        match r {
            Ok(()) => {
                return Ok(Extracted {
                    archive: archive.path.clone(),
                    dest,
                    password_used: pw.is_some(),
                });
            }
            Err(Error::Password) => {
                // Deja la carpeta vacía para el siguiente intento.
                let _ = std::fs::remove_dir_all(&dest);
                last = Error::Password;
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dest);
                last = e;
                break;
            }
        }
    }
    Err(last)
}

/// `dir/stem`, o `dir/stem (2)`… si ya existe y no está vacía.
fn unique_dir(dir: &Path, stem: &str) -> PathBuf {
    let stem = if stem.is_empty() {
        "descomprimido"
    } else {
        stem
    };
    let mut p = dir.join(stem);
    let mut n = 2;
    while p.exists() && std::fs::read_dir(&p).is_ok_and(|mut r| r.next().is_some()) {
        p = dir.join(format!("{stem} ({n})"));
        n += 1;
    }
    p
}

/// La ruta de un archivo de dentro, solo si queda dentro de `dest`.
pub(crate) fn safe_join(dest: &Path, name: &str) -> Option<PathBuf> {
    let rel = Path::new(name);
    let mut out = dest.to_path_buf();
    for c in rel.components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (out != dest).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[Archive]) -> Vec<String> {
        v.iter()
            .map(|a| a.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn only_entry_points_are_picked() {
        let files = [
            "juego.part1.rar",
            "juego.part2.rar",
            "juego.part3.rar",
            "otro.part01.rar",
            "otro.part02.rar",
            "suelto.rar",
            "datos.zip",
            "copia.7z",
            "vol.7z.001",
            "vol.7z.002",
            "leeme.txt",
            "viejo.r00",
        ];
        let paths: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
        let got = entry_points(paths.iter().map(PathBuf::as_path));
        assert_eq!(
            names(&got),
            [
                "copia.7z",
                "datos.zip",
                "juego.part1.rar",
                "otro.part01.rar",
                "suelto.rar",
                "vol.7z.001"
            ]
        );
        let stems: Vec<_> = got.iter().map(|a| a.stem.as_str()).collect();
        assert_eq!(stems, ["copia", "datos", "juego", "otro", "suelto", "vol"]);
    }

    #[test]
    fn names_cannot_escape() {
        let d = Path::new("/tmp/x");
        assert_eq!(
            safe_join(d, "a/b.txt"),
            Some(PathBuf::from("/tmp/x/a/b.txt"))
        );
        assert_eq!(safe_join(d, "../b.txt"), None);
        assert_eq!(safe_join(d, "/etc/passwd"), None);
        assert_eq!(safe_join(d, "a/../../b"), None);
        assert_eq!(safe_join(d, ""), None);
    }

    #[test]
    fn rar_parts_are_found() {
        let dir = tempfile::tempdir().unwrap();
        for n in [
            "g.part1.rar",
            "g.part2.rar",
            "g.part3.rar",
            "h.part1.rar",
            "x.txt",
        ] {
            std::fs::write(dir.path().join(n), b"x").unwrap();
        }
        let a = classify(&dir.path().join("g.part1.rar")).unwrap();
        let parts = parts_of(&a);
        let n: Vec<_> = parts
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(n, ["g.part1.rar", "g.part2.rar", "g.part3.rar"]);
    }
}
