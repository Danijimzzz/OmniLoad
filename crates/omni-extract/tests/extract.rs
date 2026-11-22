//! Extracción de verdad: se crean archivos ZIP y 7z (con y sin contraseña) y
//! se comprueba qué sale de ellos.

use std::io::Write;
use std::path::{Path, PathBuf};

use omni_extract::{Error, classify, extract};
use zip::write::SimpleFileOptions;

fn make_zip(path: &Path, files: &[(&str, &[u8])], password: Option<&str>) {
    let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    for (name, data) in files {
        let mut opts = SimpleFileOptions::default();
        if let Some(pw) = password {
            opts = opts.with_aes_encryption(zip::AesMode::Aes256, pw);
        }
        w.start_file(*name, opts).unwrap();
        w.write_all(data).unwrap();
    }
    w.finish().unwrap();
}

fn make_7z(dir: &Path, path: &Path, password: Option<&str>) {
    let src = dir.join("src7");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(src.join("a.txt"), b"hola").unwrap();
    std::fs::write(src.join("sub").join("b.txt"), b"adios").unwrap();
    match password {
        Some(pw) => sevenz_rust2::compress_to_path_encrypted(&src, path, pw.into()).unwrap(),
        None => sevenz_rust2::compress_to_path(&src, path).unwrap(),
    }
}

fn read(p: PathBuf) -> String {
    String::from_utf8(std::fs::read(p).unwrap()).unwrap()
}

#[test]
fn plain_zip() {
    let dir = tempfile::tempdir().unwrap();
    let z = dir.path().join("datos.zip");
    make_zip(&z, &[("a.txt", b"hola"), ("carpeta/b.txt", b"adios")], None);
    let out = extract(&classify(&z).unwrap(), &[]).unwrap();
    assert_eq!(out.dest, dir.path().join("datos"));
    assert!(!out.password_used);
    assert_eq!(read(out.dest.join("a.txt")), "hola");
    assert_eq!(read(out.dest.join("carpeta").join("b.txt")), "adios");
}

#[test]
fn zip_with_the_right_password_from_the_list() {
    let dir = tempfile::tempdir().unwrap();
    let z = dir.path().join("secreto.zip");
    make_zip(&z, &[("a.txt", b"hola")], Some("s3creto"));
    let list = vec!["otra".to_owned(), "s3creto".to_owned()];
    let out = extract(&classify(&z).unwrap(), &list).unwrap();
    assert!(out.password_used);
    assert_eq!(read(out.dest.join("a.txt")), "hola");
}

#[test]
fn zip_with_no_valid_password_fails_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let z = dir.path().join("secreto.zip");
    make_zip(&z, &[("a.txt", b"hola")], Some("s3creto"));
    let err = extract(&classify(&z).unwrap(), &["mal".to_owned()]).unwrap_err();
    assert!(matches!(err, Error::Password), "{err:?}");
    // No deja una carpeta a medias.
    assert!(!dir.path().join("secreto").exists());
}

#[test]
fn zip_cannot_write_outside() {
    let dir = tempfile::tempdir().unwrap();
    let z = dir.path().join("mala.zip");
    make_zip(&z, &[("../fuera.txt", b"x"), ("ok.txt", b"ok")], None);
    let out = extract(&classify(&z).unwrap(), &[]).unwrap();
    assert!(!dir.path().join("fuera.txt").exists());
    assert_eq!(read(out.dest.join("ok.txt")), "ok");
}

#[test]
fn plain_7z() {
    let dir = tempfile::tempdir().unwrap();
    let z = dir.path().join("copia.7z");
    make_7z(dir.path(), &z, None);
    let out = extract(&classify(&z).unwrap(), &[]).unwrap();
    let base = out.dest.join("src7");
    let found = if base.exists() {
        base
    } else {
        out.dest.clone()
    };
    assert_eq!(read(found.join("a.txt")), "hola");
    assert_eq!(read(found.join("sub").join("b.txt")), "adios");
}

#[test]
fn encrypted_7z_with_password_list() {
    let dir = tempfile::tempdir().unwrap();
    let z = dir.path().join("secreto.7z");
    make_7z(dir.path(), &z, Some("clave"));
    assert!(matches!(
        extract(&classify(&z).unwrap(), &["mala".to_owned()]),
        Err(Error::Password)
    ));
    let out = extract(
        &classify(&z).unwrap(),
        &["mala".to_owned(), "clave".to_owned()],
    )
    .unwrap();
    assert!(out.password_used);
}

#[test]
fn second_run_does_not_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let z = dir.path().join("datos.zip");
    make_zip(&z, &[("a.txt", b"hola")], None);
    let a = extract(&classify(&z).unwrap(), &[]).unwrap();
    let b = extract(&classify(&z).unwrap(), &[]).unwrap();
    assert_ne!(a.dest, b.dest);
    assert_eq!(b.dest, dir.path().join("datos (2)"));
}

/// Solo corre donde hay un programa que abre RAR: se comprueba que, sin RAR
/// de verdad, dice qué falta o falla con un mensaje y no se cuelga.
#[test]
fn bogus_rar_gives_an_error_not_a_hang() {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path().join("falso.rar");
    std::fs::write(&r, b"esto no es un rar").unwrap();
    let err = extract(&classify(&r).unwrap(), &[]).unwrap_err();
    assert!(
        matches!(err, Error::NoTool(_) | Error::Failed(_) | Error::Password),
        "{err:?}"
    );
}
