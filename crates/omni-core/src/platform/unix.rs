use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

pub fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    file.write_all_at(buf, offset)
}

pub fn is_unsupported_os_error(e: &io::Error) -> bool {
    // EOPNOTSUPP (95 en Linux, 102 en macOS) y ENOTSUP (45 en macOS).
    matches!(e.raw_os_error(), Some(95) | Some(102) | Some(45))
}
