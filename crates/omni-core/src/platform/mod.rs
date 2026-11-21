//! Código propio de cada sistema operativo. El resto del núcleo solo usa las
//! funciones de este módulo, nunca `cfg(windows)` o `cfg(unix)` directamente.

use std::fs::File;
use std::io;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as imp;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as imp;

#[cfg(not(any(unix, windows)))]
compile_error!("OmniLoad solo compila en Windows, Linux y macOS (u otros Unix)");

/// Escribe `buf` entero en la posición `offset` sin mover el cursor
/// compartido del archivo, así varias tareas pueden escribir a la vez sobre el
/// mismo `File` (`pwrite` en Unix, `WriteFile` con desplazamiento en Windows).
pub fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    imp::write_all_at(file, buf, offset)
}

/// Reserva `len` bytes en disco para detectar la falta de espacio al empezar
/// y no a mitad de descarga, y fija el tamaño del archivo a `len`.
pub fn preallocate(file: &File, len: u64) -> io::Result<()> {
    if len > 0 {
        match fs4::FileExt::allocate(file, len) {
            Ok(()) => {}
            // Sistemas de archivos sin reserva (algunos de red, FAT): seguimos
            // sin ella; set_len basta para que las escrituras posicionales
            // funcionen.
            Err(e) if is_unsupported(&e) => {}
            Err(e) => return Err(e),
        }
    }
    file.set_len(len)
}

fn is_unsupported(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::Unsupported || imp::is_unsupported_os_error(e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom};

    #[test]
    fn positional_writes_out_of_order() {
        let mut tmp = tempfile::tempfile().unwrap();
        preallocate(&tmp, 10).unwrap();
        write_all_at(&tmp, b"world", 5).unwrap();
        write_all_at(&tmp, b"hello", 0).unwrap();
        // En Windows seek_write mueve el cursor del archivo; el motor nunca lo
        // usa, pero esta lectura sí, así que volvemos al principio.
        tmp.seek(SeekFrom::Start(0)).unwrap();
        let mut s = String::new();
        tmp.read_to_string(&mut s).unwrap();
        assert_eq!(s, "helloworld");
    }
}
