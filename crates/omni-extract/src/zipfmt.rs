//! ZIP, con o sin contraseña (ZipCrypto y AES).

use std::fs::File;
use std::io::{self, BufWriter};
use std::path::Path;

use zip::ZipArchive;
use zip::result::ZipError;

use crate::{Error, Result, safe_join};

fn map(e: ZipError) -> Error {
    match e {
        ZipError::InvalidPassword => Error::Password,
        ZipError::UnsupportedArchive(m) if m == ZipError::PASSWORD_REQUIRED => Error::Password,
        ZipError::Io(e) => Error::Io(e),
        other => Error::Failed(format!("ZIP dañado o no compatible: {other}")),
    }
}

pub(crate) fn extract(archive: &Path, dest: &Path, password: Option<&str>) -> Result<()> {
    let mut zip = ZipArchive::new(File::open(archive)?).map_err(map)?;
    for i in 0..zip.len() {
        let mut entry = match password {
            Some(p) => zip.by_index_decrypt(i, p.as_bytes()),
            None => zip.by_index(i),
        }
        .map_err(map)?;
        let name = entry
            .name()
            .map_err(|e| Error::Failed(e.to_string()))?
            .into_owned();
        let Some(out) = safe_join(dest, &name) else {
            continue; // nombre peligroso: se salta
        };
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut w = BufWriter::new(File::create(&out)?);
        // Con ZipCrypto una contraseña mala puede pasar la primera
        // comprobación y fallar al leer (CRC): también cuenta como mala.
        io::copy(&mut entry, &mut w).map_err(|e| {
            if password.is_some() || entry_encrypted(&e) {
                Error::Password
            } else {
                Error::Io(e)
            }
        })?;
    }
    Ok(())
}

fn entry_encrypted(e: &io::Error) -> bool {
    e.to_string().to_ascii_lowercase().contains("password")
}
