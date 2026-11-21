use std::fs::File;
use std::io;
use std::os::windows::fs::FileExt;

pub fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    // seek_write puede escribir menos de lo pedido; repetimos hasta terminar.
    while !buf.is_empty() {
        match file.seek_write(buf, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "no se pudo escribir en el archivo",
                ));
            }
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

pub fn is_unsupported_os_error(e: &io::Error) -> bool {
    // ERROR_INVALID_FUNCTION (1) y ERROR_NOT_SUPPORTED (50).
    matches!(e.raw_os_error(), Some(1) | Some(50))
}
