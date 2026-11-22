//! Capturador de enlaces: averigua qué archivos hay detrás de un enlace antes
//! de descargar nada, como el "LinkGrabber" de JDownloader.
//!
//! - [`inspect`] mira un enlace: si es un archivo, su nombre, tamaño y si se
//!   puede reanudar; si es una página HTML, los enlaces a archivos que tiene.
//!   No sigue páginas dentro de páginas: un nivel basta para listados de
//!   carpetas y páginas de descarga sencillas.
//! - Los hosts que esconden el archivo (Mega, Gofile, Google Drive,
//!   Mediafire…) tendrán un plugin que implementa [`HostChecker`] (fase 5).
//!   [`Checkers`] reparte cada enlace a su plugin o a la comprobación
//!   genérica.
//! - [`Grabber`] es la lista del capturador: paquetes, selección, tandas de
//!   enlaces copiados, duplicados y comprobación en paralelo (con un tope
//!   por servidor para no provocar bloqueos).
//! - [`extract_urls`], [`clipboard_urls`] y [`links_from_file`] sacan
//!   enlaces de texto, del portapapeles (también del HTML copiado de una web)
//!   y de archivos `.txt`/`.html`.

mod check;
mod grabber;
mod import;
mod inspect;
mod links;

pub use check::{CheckFuture, Checkers, DEFAULT_PER_HOST, HostChecker};
pub use grabber::{
    Added, Check, Dest, FileRule, GrabCommand, GrabItem, GrabPackage, Grabber, KnownLinks,
    PARALLEL_CHECKS, Pick, Picked, Saved, package_name, short_error,
};
pub use import::{LINK_FILE_EXTENSIONS, MAX_LINK_FILE, links_from_file};
pub use inspect::{FolderFile, Found, MAX_LINKS_PER_PAGE, inspect};
pub use links::{
    FILE_HOSTS, clipboard_urls, extract_hrefs, extract_links, extract_urls, is_file_host,
    page_title,
};
pub use omni_core::RemoteInfo;
