//! Servidor HTTP falso para las pruebas. Sirve el mismo contenido en cualquier
//! ruta y puede simular lo que hacen los servidores reales cuando fallan:
//! cortar conexiones a mitad, responder 503, no aceptar rangos, ir lento o
//! cambiar el archivo (y su ETag) entre peticiones.

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use tokio::net::TcpListener;

/// Comportamiento del servidor.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// ¿Acepta `Range`?
    pub ranges: bool,
    /// Cuántas de las primeras peticiones responden 503.
    pub fail_first: u32,
    /// Cuántas respuestas se cortan a mitad.
    pub cuts: u32,
    /// Bytes que se envían antes de cortar.
    pub cut_after: u64,
    /// Tamaño de cada trozo del cuerpo.
    pub chunk: usize,
    /// Pausa antes de cada trozo (para simular un servidor lento).
    pub chunk_delay: Option<Duration>,
    pub content_disposition: Option<String>,
    /// Máximo de respuestas simultáneas; las de más reciben 429.
    pub max_concurrent: Option<u32>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            ranges: true,
            fail_first: 0,
            cuts: 0,
            cut_after: 0,
            chunk: 16 * 1024,
            chunk_delay: None,
            content_disposition: None,
            max_concurrent: None,
        }
    }
}

struct Inner {
    content: RwLock<(Bytes, String)>,
    cfg: RwLock<ServerConfig>,
    fail_left: AtomicU32,
    cuts_left: AtomicU32,
    served: AtomicU64,
    requests: AtomicU64,
    active: AtomicU32,
    rejected: AtomicU64,
    ranges: Mutex<Vec<(u64, u64)>>,
}

/// Servidor en marcha en `127.0.0.1` con un puerto libre.
pub struct TestServer {
    addr: SocketAddr,
    inner: Arc<Inner>,
    task: tokio::task::JoinHandle<()>,
}

impl TestServer {
    pub async fn start(content: Vec<u8>, cfg: ServerConfig) -> Self {
        let inner = Arc::new(Inner {
            content: RwLock::new((Bytes::from(content), "\"v1\"".to_string())),
            fail_left: AtomicU32::new(cfg.fail_first),
            cuts_left: AtomicU32::new(cfg.cuts),
            cfg: RwLock::new(cfg),
            served: AtomicU64::new(0),
            requests: AtomicU64::new(0),
            active: AtomicU32::new(0),
            rejected: AtomicU64::new(0),
            ranges: Mutex::new(Vec::new()),
        });
        let app = Router::new().fallback(handler).with_state(inner.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("servidor de pruebas");
        });
        Self { addr, inner, task }
    }

    /// URL de un archivo con el nombre dado.
    pub fn url(&self, name: &str) -> String {
        format!("http://{}/{name}", self.addr)
    }

    /// Cambia el contenido y su ETag.
    pub fn set_content(&self, content: Vec<u8>, etag: &str) {
        *self.inner.content.write().unwrap() = (Bytes::from(content), format!("\"{etag}\""));
    }

    pub fn set_chunk_delay(&self, delay: Option<Duration>) {
        self.inner.cfg.write().unwrap().chunk_delay = delay;
    }

    /// Bytes de cuerpo enviados en total.
    pub fn bytes_served(&self) -> u64 {
        self.inner.served.load(Ordering::SeqCst)
    }

    /// Respuestas 429 por exceso de conexiones simultáneas.
    pub fn rejected(&self) -> u64 {
        self.inner.rejected.load(Ordering::SeqCst)
    }

    pub fn requests(&self) -> u64 {
        self.inner.requests.load(Ordering::SeqCst)
    }

    /// Rangos pedidos (inicio, fin inclusivo), en orden de llegada.
    pub fn ranges(&self) -> Vec<(u64, u64)> {
        self.inner.ranges.lock().unwrap().clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Bytes pseudoaleatorios deterministas (xorshift), para no depender de rand.
pub fn pseudo_random_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Resta 1 al contador si es mayor que 0 y dice si lo hizo. Escrito a mano
/// porque `fetch_update` está obsoleto en Rust reciente y su sustituto
/// (`try_update`) no existe en nuestra versión mínima.
fn take_one(counter: &AtomicU32) -> bool {
    let mut cur = counter.load(Ordering::SeqCst);
    loop {
        if cur == 0 {
            return false;
        }
        match counter.compare_exchange(cur, cur - 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(actual) => cur = actual,
        }
    }
}

/// Ocupa un hueco de `max_concurrent` mientras vive la respuesta.
struct ActiveSlot(Arc<Inner>);

impl Drop for ActiveSlot {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

fn parse_range(headers: &HeaderMap) -> Option<(u64, Option<u64>)> {
    let v = headers.get(header::RANGE)?.to_str().ok()?;
    let (s, e) = v.strip_prefix("bytes=")?.split_once('-')?;
    let start = s.trim().parse().ok()?;
    let end = match e.trim() {
        "" => None,
        e => Some(e.parse().ok()?),
    };
    Some((start, end))
}

async fn handler(State(st): State<Arc<Inner>>, headers: HeaderMap) -> Response {
    st.requests.fetch_add(1, Ordering::SeqCst);
    if take_one(&st.fail_left) {
        return (StatusCode::SERVICE_UNAVAILABLE, "ocupado").into_response();
    }
    let cfg = st.cfg.read().unwrap().clone();
    let slot = match cfg.max_concurrent {
        Some(max) => {
            let mut cur = st.active.load(Ordering::SeqCst);
            loop {
                if cur >= max {
                    st.rejected.fetch_add(1, Ordering::SeqCst);
                    return (StatusCode::TOO_MANY_REQUESTS, "demasiadas conexiones")
                        .into_response();
                }
                match st
                    .active
                    .compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst)
                {
                    Ok(_) => break Some(ActiveSlot(st.clone())),
                    Err(actual) => cur = actual,
                }
            }
        }
        None => None,
    };
    let (data, etag) = st.content.read().unwrap().clone();
    let len = data.len() as u64;

    let mut range = if cfg.ranges {
        parse_range(&headers)
    } else {
        None
    };
    if let Some(ir) = headers.get(header::IF_RANGE)
        && ir.as_bytes() != etag.as_bytes()
    {
        range = None;
    }

    let mut resp_headers = HeaderMap::new();
    resp_headers.insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
    if cfg.ranges {
        resp_headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    }
    if let Some(cd) = &cfg.content_disposition {
        resp_headers.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_str(cd).unwrap(),
        );
    }

    let (status, start, end) = match range {
        Some((s, _)) if s >= len => {
            resp_headers.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{len}")).unwrap(),
            );
            return (StatusCode::RANGE_NOT_SATISFIABLE, resp_headers).into_response();
        }
        Some((s, e)) => {
            let e = e.map_or(len, |e| (e + 1).min(len));
            st.ranges.lock().unwrap().push((s, e - 1));
            resp_headers.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes {s}-{}/{len}", e - 1)).unwrap(),
            );
            (StatusCode::PARTIAL_CONTENT, s, e)
        }
        None => (StatusCode::OK, 0, len),
    };
    resp_headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&(end - start).to_string()).unwrap(),
    );

    let cut_at = take_one(&st.cuts_left).then_some(start + cfg.cut_after);

    let body_state = (start, data, st.clone(), false, slot);
    let stream = futures_util::stream::unfold(body_state, move |(pos, data, st, done, slot)| {
        let cfg = cfg.clone();
        async move {
            if done || pos >= end {
                return None;
            }
            if let Some(cut) = cut_at
                && pos >= cut
            {
                let err = io::Error::new(io::ErrorKind::ConnectionReset, "corte simulado");
                return Some((Err(err), (pos, data, st, true, slot)));
            }
            if let Some(d) = cfg.chunk_delay {
                tokio::time::sleep(d).await;
            }
            let mut next = (pos + cfg.chunk as u64).min(end);
            if let Some(cut) = cut_at {
                next = next.min(cut.max(pos + 1));
            }
            let chunk = data.slice(pos as usize..next as usize);
            st.served.fetch_add(next - pos, Ordering::SeqCst);
            Some((Ok::<_, io::Error>(chunk), (next, data, st, false, slot)))
        }
    });
    (status, resp_headers, Body::from_stream(stream)).into_response()
}
