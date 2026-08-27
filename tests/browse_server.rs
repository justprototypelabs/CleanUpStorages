use std::io::{Read, Write};
use std::net::TcpStream;

// Start the browse server on an ephemeral port in a background thread, return its addr.
fn start_server() -> std::net::SocketAddr {
    use std::sync::mpsc;
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("c.db");
    {
        let cat = cleanupstorages::catalog::Catalog::open(&db).unwrap();
        cat.upsert_volume(&cleanupstorages::catalog::models::Volume {
            volume_id: "vol-1".into(),
            label: "Test HDD".into(),
            identified_by: "marker".into(),
            first_seen_at: 1,
            last_seen_at: 1,
        })
        .unwrap();
        cat.upsert_file(
            &cleanupstorages::catalog::models::NewFile {
                volume_id: "vol-1".into(),
                relative_path: "docs/thesis.pdf".into(),
                filename: "thesis.pdf".into(),
                extension: "pdf".into(),
                size_bytes: 5,
                content_hash: "h1".into(),
                created_time: None,
                modified_time: None,
                accessed_time: None,
                category: cleanupstorages::category::Category::Document,
                container_chain: None,
            },
            100,
        )
        .unwrap();
        // An archive entry, so `/api/archives` has something to report on `vol-1` -- which this
        // fixture never mounts, so it exercises the "offline drive" path.
        cat.upsert_file(
            &cleanupstorages::catalog::models::NewFile {
                volume_id: "vol-1".into(),
                relative_path: "archives/bundle.zip".into(),
                filename: "notes.txt".into(),
                extension: "txt".into(),
                size_bytes: 5,
                content_hash: "h2".into(),
                created_time: None,
                modified_time: None,
                accessed_time: None,
                category: cleanupstorages::category::Category::Document,
                container_chain: Some("notes.txt".into()),
            },
            100,
        )
        .unwrap();
    }
    // keep tmp alive for the whole test process
    std::mem::forget(tmp);

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let app = cleanupstorages::web::build_router(db);
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            axum::serve(listener, app).await.unwrap();
        });
    });
    rx.recv().unwrap()
}

fn http_get(addr: std::net::SocketAddr, path: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(s, "GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    buf
}

fn http_post_json(addr: std::net::SocketAddr, path: &str, body: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(
        s,
        "POST {path} HTTP/1.0\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    buf
}

#[test]
fn server_serves_page_and_search() {
    let addr = start_server();
    let index = http_get(addr, "/");
    assert!(index.contains("200 OK"));
    assert!(index.contains("CleanUpStorages"));

    let search = http_get(addr, "/api/search?q=thesis");
    assert!(search.contains("200 OK"));
    assert!(search.contains("docs/thesis.pdf"), "search body: {search}");
}

#[test]
fn archives_endpoint_reports_scope_and_refuses_to_guess_for_an_offline_drive() {
    let addr = start_server(); // fixture includes an archive entry on the never-mounted "vol-1"
    let body = http_get(addr, "/api/archives");
    assert!(body.contains("200 OK"));
    assert!(
        body.contains("\"connected\":false"),
        "offline drive must say so: {body}"
    );
    // Must be the literal null, not a defaulted false -- a regression that serialized "no verdict"
    // as false would be indistinguishable from a real refusal, which is the exact bug the nullable
    // field exists to prevent.
    assert!(
        body.contains("\"in_scope\":null"),
        "offline drive must issue no verdict at all: {body}"
    );
    assert!(
        !body.contains("\"in_scope\":false"),
        "no verdict may be issued without a live mount: {body}"
    );
    assert!(
        !body.contains("\"in_scope\":true"),
        "no verdict may be issued without a live mount: {body}"
    );
}

#[test]
fn extract_endpoint_requires_csrf_token() {
    let addr = start_server();
    let body = http_post_json(
        addr,
        "/api/extract",
        r#"{"volume_id":"vol-1","paths":["archives/bundle.zip"]}"#,
    );
    assert!(
        body.contains("403 Forbidden"),
        "a request without the CSRF token must be rejected before touching the queue: {body}"
    );
}

#[test]
fn extract_page_renders_and_is_in_the_nav() {
    let addr = start_server();
    let body = http_get(addr, "/extract");
    assert!(body.contains("200 OK"));
    assert!(body.contains("Extract"), "page title present");
    assert!(body.contains("href=\"/extract\""), "nav entry present");
    let overview = http_get(addr, "/");
    assert!(
        overview.contains("href=\"/extract\""),
        "nav is on every page"
    );
}
