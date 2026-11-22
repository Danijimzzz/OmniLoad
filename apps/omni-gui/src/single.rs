//! Una sola ventana de OmniLoad a la vez.
//!
//! Con la ventana escondida en la bandeja, abrir OmniLoad otra vez (desde el
//! menú de inicio, un acceso directo o «Abrir con» sobre una lista de
//! enlaces) no debe abrir una segunda ventana: la nueva le pasa sus
//! argumentos a la que ya está en marcha, que se enseña, y termina.
//!
//! La ventana en marcha escucha en `127.0.0.1` en un puerto libre y lo apunta,
//! con un token aleatorio, en `ventana.json` junto a los ajustes (legible solo
//! por el usuario en Unix). Quien no sepa el token no puede mandarle nada.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Duration;

use omni_api::Endpoint;

/// Cuánto esperar a la otra ventana antes de abrir una nueva.
const TIMEOUT: Duration = Duration::from_secs(2);
/// Lo más que se lee de una petición (son unos pocos enlaces o rutas).
const MAX_REQUEST: u64 = 4 << 20;

/// Resultado de intentar ser la única ventana.
pub enum Claim {
    /// Ya había una: se le pasaron los argumentos. Hay que terminar.
    Forwarded,
    /// Esta es la ventana. Mientras viva el valor se atienden peticiones.
    Primary(Instance),
}

/// La ventana en marcha: borra `ventana.json` al soltarla.
pub struct Instance {
    path: PathBuf,
    token: String,
}

impl Drop for Instance {
    fn drop(&mut self) {
        // Solo si sigue siendo el nuestro (otra ventana pudo ocupar el sitio).
        if Endpoint::read(&self.path).is_some_and(|e| e.token == self.token) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Busca otra ventana y le pasa `args`; si no hay, se queda con el sitio y
/// manda a `tx` los argumentos que lleguen de futuras ventanas (una lista
/// vacía = solo enseñar la ventana).
pub fn claim(path: &Path, args: &[String], tx: Sender<Vec<String>>) -> Claim {
    if let Some(ep) = Endpoint::read(path) {
        match forward(&ep, args) {
            Ok(()) => return Claim::Forwarded,
            Err(e) => tracing::debug!("la otra ventana no contesta ({e}): se abre esta"),
        }
    }
    match listen(path, tx) {
        Ok(inst) => Claim::Primary(inst),
        Err(e) => {
            // Sin esto todo funciona igual, salvo que se podrían abrir dos
            // ventanas.
            tracing::warn!("no se pudo preparar la ventana única: {e}");
            Claim::Primary(Instance {
                path: path.to_owned(),
                token: String::new(),
            })
        }
    }
}

fn forward(ep: &Endpoint, args: &[String]) -> std::io::Result<()> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, ep.port));
    let mut s = TcpStream::connect_timeout(&addr, TIMEOUT)?;
    s.set_read_timeout(Some(TIMEOUT))?;
    s.set_write_timeout(Some(TIMEOUT))?;
    let mut msg = format!("{}\n", ep.token);
    for a in args {
        // Las rutas relativas se resuelven aquí: la otra ventana tiene otra
        // carpeta actual.
        let p = Path::new(a);
        let a = if p.is_file() {
            std::fs::canonicalize(p).map_or_else(|_| a.clone(), |p| p.display().to_string())
        } else {
            a.clone()
        };
        msg.push_str(&a.replace(['\r', '\n'], " "));
        msg.push('\n');
    }
    s.write_all(msg.as_bytes())?;
    s.shutdown(std::net::Shutdown::Write)?;
    let mut reply = String::new();
    s.take(16).read_to_string(&mut reply)?;
    if reply.trim() == "ok" {
        Ok(())
    } else {
        Err(std::io::Error::other("respuesta inesperada"))
    }
}

fn listen(path: &Path, tx: Sender<Vec<String>>) -> std::io::Result<Instance> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let port = listener.local_addr()?.port();
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).map_err(|e| std::io::Error::other(e.to_string()))?;
    let token = hex::encode(buf);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    Endpoint {
        port,
        token: token.clone(),
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").into(),
    }
    .write(path)?;

    let expected = token.clone();
    std::thread::Builder::new()
        .name("ventana-unica".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                match serve(stream, &expected) {
                    Some(args) => {
                        if tx.send(args).is_err() {
                            break;
                        }
                    }
                    None => tracing::debug!("petición rechazada en la ventana única"),
                }
            }
        })?;
    Ok(Instance {
        path: path.to_owned(),
        token,
    })
}

fn serve(mut stream: TcpStream, token: &str) -> Option<Vec<String>> {
    stream.set_read_timeout(Some(TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(TIMEOUT)).ok()?;
    let mut lines = BufReader::new((&stream).take(MAX_REQUEST)).lines();
    if lines.next()?.ok()? != token {
        return None;
    }
    let args: Vec<String> = lines
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .collect();
    stream.write_all(b"ok\n").ok()?;
    Some(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn second_window_forwards_its_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ventana.json");
        let (tx, rx) = mpsc::channel();
        let Claim::Primary(first) = claim(&path, &[], tx) else {
            panic!("la primera ventana debe quedarse");
        };
        assert!(path.exists());

        let (tx2, _rx2) = mpsc::channel();
        let args = vec!["https://a.com/x.zip".to_owned(), "otra\nlínea".to_owned()];
        assert!(matches!(claim(&path, &args, tx2), Claim::Forwarded));
        let got = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(got, vec!["https://a.com/x.zip", "otra línea"]);

        // Sin argumentos: solo enseñar la ventana.
        let (tx3, _rx3) = mpsc::channel();
        assert!(matches!(claim(&path, &[], tx3), Claim::Forwarded));
        assert!(rx.recv_timeout(Duration::from_secs(5)).unwrap().is_empty());

        drop(first);
        assert!(!path.exists());
    }

    #[test]
    fn wrong_token_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ventana.json");
        let (tx, rx) = mpsc::channel();
        let Claim::Primary(_first) = claim(&path, &[], tx) else {
            panic!();
        };
        let mut ep = Endpoint::read(&path).unwrap();
        ep.token = "malo".into();
        assert!(forward(&ep, &["https://a.com".into()]).is_err());
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
    }

    #[test]
    fn stale_file_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ventana.json");
        // Una ventana que se cerró de golpe y dejó el archivo.
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        Endpoint {
            port,
            token: "viejo".into(),
            pid: 1,
            version: "0".into(),
        }
        .write(&path)
        .unwrap();
        let (tx, _rx) = mpsc::channel();
        let Claim::Primary(_inst) = claim(&path, &[], tx) else {
            panic!("el archivo viejo no debe impedir abrir la ventana");
        };
        assert_ne!(Endpoint::read(&path).unwrap().token, "viejo");
    }
}
