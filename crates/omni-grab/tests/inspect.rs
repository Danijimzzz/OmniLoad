//! `inspect` contra un servidor local: un archivo y una página con enlaces.

use axum::Router;
use axum::http::header;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use omni_core::Network;
use omni_grab::{Found, inspect};
use url::Url;

async fn start() -> String {
    let app = Router::new()
        .route(
            "/",
            get(|| async {
                Html(
                    r#"<html><body>
                    <a href="a.bin">A</a> <a href="/sub/b.zip">B</a>
                    <a href="otra.html">página</a>
                    </body></html>"#,
                )
            }),
        )
        .route(
            "/a.bin",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "application/octet-stream")],
                    vec![7u8; 1234],
                )
            }),
        )
        .route(
            "/texto",
            get(|| async { ([(header::CONTENT_TYPE, "text/plain")], "hola").into_response() }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn file_and_page() {
    let base = start().await;
    let net = Network::system_default().unwrap();
    let client = net.client();

    let file = inspect(client, &Url::parse(&format!("{base}/a.bin")).unwrap())
        .await
        .unwrap();
    match file {
        Found::File(info) => {
            assert_eq!(info.filename, "a.bin");
            assert_eq!(info.size, Some(1234));
        }
        other => panic!("esperaba un archivo: {other:?}"),
    }

    let page = inspect(client, &Url::parse(&format!("{base}/")).unwrap())
        .await
        .unwrap();
    match page {
        Found::Page { links, .. } => {
            let links: Vec<String> = links.into_iter().map(String::from).collect();
            assert_eq!(
                links,
                [format!("{base}/a.bin"), format!("{base}/sub/b.zip")]
            );
        }
        other => panic!("esperaba una página: {other:?}"),
    }

    // Un texto plano no es una página: se trata como archivo.
    let txt = inspect(client, &Url::parse(&format!("{base}/texto")).unwrap())
        .await
        .unwrap();
    assert!(matches!(txt, Found::File(_)));

    // 404: error del sondeo (enlace caducado).
    let err = inspect(
        client,
        &Url::parse(&format!("{base}/no-existe.zip")).unwrap(),
    )
    .await;
    assert!(err.is_err());
}
