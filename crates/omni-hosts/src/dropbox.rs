//! Dropbox: un enlace compartido con `dl=1` redirige a la descarga directa
//! (`dl.dropboxusercontent.com`). Las carpetas se bajan como un ZIP.

use omni_core::Error;
use omni_grab::{CheckFuture, Found};
use omni_queue::ResolveFuture;
use url::Url;

use crate::{Host, direct, on_domain, segments};

const NAME: &str = "Dropbox";

/// Enlaces compartidos: `/s/`, `/sh/`, `/scl/fi/` (archivo), `/scl/fo/`
/// (carpeta) y `/sc/` (antiguo).
fn is_shared(url: &Url) -> bool {
    // `dl.dropboxusercontent.com` y `dl.dropbox.com` ya son directos.
    if !on_domain(url, "dropbox.com") || url.host_str().is_some_and(|h| h.starts_with("dl.")) {
        return false;
    }
    let seg = segments(url);
    matches!(
        seg.iter().map(String::as_str).collect::<Vec<_>>()[..],
        ["s" | "sh" | "sc", _, ..] | ["scl", "fi" | "fo", _, ..]
    )
}

/// El mismo enlace con `dl=1` (y sin `raw`, que lo desvía a la vista previa).
pub(crate) fn download_url(url: &Url) -> Url {
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != "dl" && k != "raw")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let mut out = url.clone();
    out.set_fragment(None);
    {
        let mut q = out.query_pairs_mut();
        q.clear();
        q.extend_pairs(&kept);
        q.append_pair("dl", "1");
    }
    out
}

/// Dropbox.
#[derive(Default)]
pub struct Dropbox;

impl Dropbox {
    pub fn new() -> Self {
        Self
    }
}

impl Host for Dropbox {
    fn name(&self) -> &str {
        NAME
    }

    fn handles(&self, url: &Url) -> bool {
        is_shared(url)
    }

    fn parallel(&self) -> usize {
        2
    }

    fn check<'a>(&'a self, client: &'a reqwest::Client, url: &'a Url) -> CheckFuture<'a> {
        Box::pin(async move {
            match omni_grab::inspect(client, &download_url(url)).await? {
                found @ Found::File(_) => Ok(found),
                // Con contraseña, o borrado: Dropbox contesta con una página.
                _ => Err(Error::host(
                    NAME,
                    "el enlace no se puede descargar (¿tiene contraseña o ya no existe?)",
                )),
            }
        })
    }

    fn resolve<'a>(&'a self, _client: &'a reqwest::Client, url: &'a Url) -> ResolveFuture<'a> {
        Box::pin(async move { Ok(direct(download_url(url))) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn recognises_shared_links() {
        for ok in [
            "https://www.dropbox.com/s/abc123/foto.jpg?dl=0",
            "https://www.dropbox.com/scl/fi/abc/foto.jpg?rlkey=k&dl=0",
            "https://www.dropbox.com/scl/fo/abc/xyz?rlkey=k&dl=0",
            "https://dropbox.com/sh/abc/AAAA?dl=0",
        ] {
            assert!(is_shared(&u(ok)), "{ok}");
        }
        for no in [
            "https://www.dropbox.com/home",
            "https://dl.dropboxusercontent.com/s/abc/foto.jpg",
            "https://dl.dropbox.com/s/abc/foto.jpg",
            "https://example.com/s/abc/x",
        ] {
            assert!(!is_shared(&u(no)), "{no}");
        }
    }

    #[test]
    fn forces_download() {
        assert_eq!(
            download_url(&u("https://www.dropbox.com/scl/fi/a/b.zip?rlkey=k&dl=0")).as_str(),
            "https://www.dropbox.com/scl/fi/a/b.zip?rlkey=k&dl=1"
        );
        assert_eq!(
            download_url(&u("https://www.dropbox.com/s/a/b.zip?raw=1#x")).as_str(),
            "https://www.dropbox.com/s/a/b.zip?dl=1"
        );
        assert_eq!(
            download_url(&u("https://www.dropbox.com/s/a/b.zip")).as_str(),
            "https://www.dropbox.com/s/a/b.zip?dl=1"
        );
    }
}
