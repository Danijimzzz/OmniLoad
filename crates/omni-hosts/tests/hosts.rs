//! Los plugins contra servidores de mentira que imitan la API de cada host.
//! Las direcciones que se comprueban tienen la forma real del host; solo la
//! dirección de su API apunta al servidor de pruebas.

use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use omni_core::{Error, Network};
use omni_grab::Found;
use omni_hosts::{ArchiveOrg, Dropbox, Hosts, Mediafire, Pixeldrain};
use omni_queue::Resolver;
use omni_testserver::{ServerConfig, TestServer, pseudo_random_bytes};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn client() -> reqwest::Client {
    Network::system_default().unwrap().client().clone()
}

fn u(s: &str) -> Url {
    Url::parse(s).unwrap()
}

/// Comprueba con el plugin que reconozca `url`.
async fn check(hosts: &Hosts, url: &str) -> omni_core::Result<Found> {
    hosts.checkers().check(&client(), &u(url)).await
}

// ---------------------------------------------------------------- Pixeldrain

fn pixeldrain_app() -> Router {
    Router::new()
        .route(
            "/api/file/{id}/info",
            get(|Path(id): Path<String>| async move {
                match id.as_str() {
                    "ok" => (
                        StatusCode::OK,
                        Json(json!({"success": true, "id": "ok", "name": "foto.jpg", "size": 1234})),
                    ),
                    "captcha" => (
                        StatusCode::FORBIDDEN,
                        Json(json!({"success": false, "value": "file_rate_limited_captcha_required"})),
                    ),
                    _ => (
                        StatusCode::NOT_FOUND,
                        Json(json!({"success": false, "value": "file_not_found"})),
                    ),
                }
            }),
        )
        .route(
            "/api/list/{id}",
            get(|Path(id): Path<String>| async move {
                if id == "lst" {
                    (
                        StatusCode::OK,
                        Json(json!({"success": true, "title": "Mis cosas", "files": [
                            {"id": "a1", "name": "uno.txt", "size": 10},
                            {"id": "b2", "name": "dos.txt", "size": 20}
                        ]})),
                    )
                } else {
                    (
                        StatusCode::NOT_FOUND,
                        Json(json!({"success": false, "value": "list_not_found"})),
                    )
                }
            }),
        )
}

#[tokio::test]
async fn pixeldrain_file_list_and_errors() {
    let api = serve(pixeldrain_app()).await;
    let hosts = Hosts::empty().with(Pixeldrain::with_base(&api));

    let Found::File(f) = check(&hosts, "https://pixeldrain.com/u/ok").await.unwrap() else {
        panic!("se esperaba un archivo")
    };
    assert_eq!(f.filename, "foto.jpg");
    assert_eq!(f.size, Some(1234));
    assert!(f.accepts_ranges);
    assert_eq!(f.url.as_str(), format!("{api}/api/file/ok"));

    let Found::Folder { name, files } =
        check(&hosts, "https://pixeldrain.com/l/lst").await.unwrap()
    else {
        panic!("se esperaba una carpeta")
    };
    assert_eq!(name.as_deref(), Some("Mis cosas"));
    assert_eq!(files.len(), 2);
    assert_eq!(files[1].size, Some(20));
    // Cada archivo de la lista se resuelve luego como un archivo normal.
    assert_eq!(files[0].url.as_str(), "https://pixeldrain.com/u/a1");

    let err = check(&hosts, "https://pixeldrain.com/u/nope")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::LinkExpired { status: 404 }), "{err}");
    let err = check(&hosts, "https://pixeldrain.com/l/nope")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::LinkExpired { status: 404 }), "{err}");
    let err = check(&hosts, "https://pixeldrain.com/u/captcha")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("captcha"), "{err}");
}

#[tokio::test]
async fn resolver_gives_direct_links_and_leaves_the_rest() {
    let api = serve(pixeldrain_app()).await;
    let hosts = Hosts::empty().with(Pixeldrain::with_base(&api));
    let resolver = hosts.resolver(client());
    let cancel = CancellationToken::new();

    let link = resolver
        .resolve("https://pixeldrain.com/u/ok", &cancel)
        .await
        .unwrap();
    assert_eq!(link.url, format!("{api}/api/file/ok"));

    let link = resolver
        .resolve("https://example.com/a.zip", &cancel)
        .await
        .unwrap();
    assert_eq!(link.url, "https://example.com/a.zip");

    let err = resolver
        .resolve("https://pixeldrain.com/l/lst", &cancel)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("lista"), "{err}");
}

// --------------------------------------------------------------- Archive.org

#[tokio::test]
async fn archive_lists_files_with_encoded_paths() {
    let app = Router::new().route(
        "/metadata/{id}",
        get(|Path(id): Path<String>| async move {
            match id.as_str() {
                "item" => Json(json!({
                    "metadata": {"title": "Un elemento"},
                    "files": [
                        {"name": "item_meta.xml", "source": "metadata", "size": "10"},
                        {"name": "video final.mp4", "source": "original", "size": "1000", "md5": "x"},
                        {"name": "extra/sub carpeta/nota#1.txt", "source": "original", "size": 5}
                    ]
                })),
                "dark" => Json(json!({"files": [], "is_dark": true})),
                "vacio" => Json(json!({"files": [{"name": "a_meta.xml", "source": "metadata"}]})),
                _ => Json(json!({})),
            }
        }),
    );
    let base = serve(app).await;
    let hosts = Hosts::empty().with(ArchiveOrg::with_base(&base));

    let Found::Folder { name, files } = check(&hosts, "https://archive.org/details/item")
        .await
        .unwrap()
    else {
        panic!("se esperaba una carpeta")
    };
    assert_eq!(name.as_deref(), Some("Un elemento"));
    assert_eq!(files.len(), 2, "el archivo de metadatos no se descarga");
    assert_eq!(files[0].name, "video final.mp4");
    assert_eq!(files[0].size, Some(1000));
    assert_eq!(
        files[0].url.as_str(),
        format!("{base}/download/item/video%20final.mp4")
    );
    assert_eq!(files[1].name, "nota#1.txt");
    assert_eq!(
        files[1].url.as_str(),
        format!("{base}/download/item/extra/sub%20carpeta/nota%231.txt")
    );

    let err = check(&hosts, "https://archive.org/details/nada")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::LinkExpired { status: 404 }), "{err}");
    let err = check(&hosts, "https://archive.org/details/dark")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("retirado"), "{err}");
    let err = check(&hosts, "https://archive.org/download/vacio")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no tiene archivos"), "{err}");
}

// ------------------------------------------------------------------- Dropbox

#[tokio::test]
async fn dropbox_forces_direct_download() {
    let hosts = Hosts::empty().with(Dropbox::new());
    let link = hosts
        .resolver(client())
        .resolve(
            "https://www.dropbox.com/scl/fi/abc/foto.jpg?rlkey=k&dl=0",
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        link.url,
        "https://www.dropbox.com/scl/fi/abc/foto.jpg?rlkey=k&dl=1"
    );
}

// ----------------------------------------------------------------- Mediafire

async fn mediafire_fixture() -> (String, TestServer) {
    let data = TestServer::start(pseudo_random_bytes(4096, 7), ServerConfig::default()).await;
    let file_url = data.url("a.bin");
    let app = Router::new()
        .route(
            "/file/{key}/file",
            get(move |Path(key): Path<String>| {
                let file_url = file_url.clone();
                async move {
                    match key.as_str() {
                        "good" => (
                            StatusCode::OK,
                            format!(
                                r#"<html><a class="input popsok" aria-label="Download file" href="{file_url}" id="downloadButton">Download (4 KB)</a></html>"#
                            ),
                        ),
                        "gone" => (
                            StatusCode::OK,
                            "<html>The key you provided for file download was invalid</html>".into(),
                        ),
                        "captcha" => (StatusCode::OK, "<div class=g-recaptcha></div>".into()),
                        _ => (StatusCode::NOT_FOUND, String::new()),
                    }
                }
            }),
        )
        .route(
            "/api/1.5/folder/get_content.php",
            get(|Query(q): Query<std::collections::HashMap<String, String>>| async move {
                let key = q["folder_key"].as_str();
                let kind = q["content_type"].as_str();
                let chunk: u32 = q.get("chunk").and_then(|c| c.parse().ok()).unwrap_or(1);
                let content: Value = match (key, kind) {
                    ("root", "files") if chunk == 1 => json!({
                        "files": [{"quickkey": "q1", "filename": "uno.zip", "size": "100"}],
                        "more_chunks": "yes"
                    }),
                    ("root", "files") => json!({
                        "files": [{"quickkey": "q2", "filename": "dos copia.zip", "size": "200"}],
                        "more_chunks": "no"
                    }),
                    ("root", "folders") => json!({
                        "folders": [{"folderkey": "sub", "name": "Sub"}]
                    }),
                    ("sub", "files") => json!({
                        "files": [{"quickkey": "q3", "filename": "tres.zip", "size": "300"}],
                        "more_chunks": "no"
                    }),
                    ("sub", "folders") => json!({"folders": []}),
                    _ => {
                        return Json(json!({"response": {"result": "Error",
                            "message": "Unknown or Invalid FolderKey"}}));
                    }
                };
                Json(json!({"response": {"result": "Success", "folder_content": content}}))
            }),
        )
        .route(
            "/api/1.5/folder/get_info.php",
            get(|| async {
                Json(json!({"response": {"result": "Success",
                    "folder_info": {"name": "Mi carpeta"}}}))
            }),
        );
    (serve(app).await, data)
}

#[tokio::test]
async fn mediafire_file_page_to_direct_link() {
    let (base, data) = mediafire_fixture().await;
    let hosts = Hosts::empty().with(Mediafire::with_base(&base));

    // La comprobación sigue el botón de descarga y sondea el archivo real.
    let Found::File(f) = check(
        &hosts,
        "https://www.mediafire.com/file/good/mi_archivo.bin/file",
    )
    .await
    .unwrap() else {
        panic!("se esperaba un archivo")
    };
    assert_eq!(f.size, Some(4096));
    assert!(f.accepts_ranges);
    assert_eq!(f.url.as_str(), data.url("a.bin"));

    let link = hosts
        .resolver(client())
        .resolve("https://www.mediafire.com/?good", &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(link.url, data.url("a.bin"));

    let err = check(&hosts, "https://www.mediafire.com/file/gone/x/file")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("ya no existe"), "{err}");
    let err = check(&hosts, "https://www.mediafire.com/file/captcha/x/file")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("captcha"), "{err}");
    let err = check(&hosts, "https://www.mediafire.com/file/other/x/file")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::LinkExpired { status: 404 }), "{err}");
}

#[tokio::test]
async fn mediafire_folder_walks_chunks_and_subfolders() {
    let (base, _data) = mediafire_fixture().await;
    let hosts = Hosts::empty().with(Mediafire::with_base(&base));

    let Found::Folder { name, files } =
        check(&hosts, "https://www.mediafire.com/folder/root/Mi_carpeta")
            .await
            .unwrap()
    else {
        panic!("se esperaba una carpeta")
    };
    assert_eq!(name.as_deref(), Some("Mi carpeta"));
    let names: Vec<_> = files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["uno.zip", "dos copia.zip", "tres.zip"]);
    assert_eq!(files[1].size, Some(200));
    assert_eq!(
        files[1].url.as_str(),
        format!("{base}/file/q2/dos%20copia.zip/file")
    );

    let err = check(&hosts, "https://www.mediafire.com/folder/mala/x")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::LinkExpired { status: 404 }), "{err}");
}

#[tokio::test]
async fn standard_set_has_the_four_hosts() {
    let hosts = Hosts::standard();
    assert_eq!(
        hosts.names(),
        ["Pixeldrain", "Archive.org", "Dropbox", "Mediafire"]
    );
    assert_eq!(hosts.checkers().names().len(), 4);
}
