//! 7z, con o sin contraseña.

use std::path::Path;

use sevenz_rust2::{ArchiveEntry, Error as SzError, Password, default_entry_extract_fn};

use crate::{Error, Result, safe_join};

pub(crate) fn extract(archive: &Path, dest: &Path, password: Option<&str>) -> Result<()> {
    let pw = password.map_or_else(Password::empty, Password::from);
    let file = std::fs::File::open(archive)?;
    let dest = dest.to_path_buf();
    let dest2 = dest.clone();
    sevenz_rust2::decompress_with_extract_fn_and_password(
        file,
        &dest,
        pw,
        move |entry: &ArchiveEntry, reader, out| {
            // Los nombres de dentro no pueden salirse de la carpeta.
            let Ok(rel) = out.strip_prefix(&dest2) else {
                return Ok(true);
            };
            let Some(safe) = safe_join(&dest2, &rel.to_string_lossy()) else {
                return Ok(true);
            };
            default_entry_extract_fn(entry, reader, &safe)
        },
    )
    .map_err(|e| match e {
        SzError::PasswordRequired | SzError::MaybeBadPassword(_) => Error::Password,
        SzError::Io(e, _) => Error::Io(e),
        other => Error::Failed(format!("7z dañado o no compatible: {other}")),
    })
}
