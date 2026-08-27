// Same harness as tests/repack_flow.rs and tests/browse_server.rs: a real binary, a real drive, a
// real catalogue, and a real HTTP server started against the data dir the CLI just wrote -- so the
// queue, the API and the extraction engine are exercised together, not mocked at any seam.
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cleanupstorages"))
}

fn write_zip(path: &std::path::Path, files: &[(&str, &[u8])]) {
    let f = std::fs::File::create(path).unwrap();
    let mut zw = zip::ZipWriter::new(f);
    let opts: zip::write::FileOptions<()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, bytes) in files {
        zw.start_file(*name, opts).unwrap();
        zw.write_all(bytes).unwrap();
    }
    zw.finish().unwrap();
}

/// Start the browse server against an EXISTING data dir (one the CLI scan already wrote), on an
/// ephemeral port, in a background thread. Adapted from `tests/browse_server.rs::start_server`,
/// which instead builds its own throwaway catalogue -- this variant reuses the catalogue at
/// `data_dir/catalog.db` so the server sees exactly what the CLI scan produced.
///
/// `build_router` alone (what `browse_server.rs` uses) never spawns the scan/quarantine queue
/// workers -- only `web::serve` does, for the real `browse` subcommand. A plain `build_router`
/// here would accept the POST, queue the job, and then let it sit forever with nothing to drain
/// it, so this spawns both workers itself, matching what `serve` does.
fn start_server_for(data_dir: &std::path::Path) -> std::net::SocketAddr {
    use std::sync::mpsc;
    let db = data_dir.join("catalog.db");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let state = cleanupstorages::web::AppState::new_live(db);
            tokio::spawn(state.scan_queue.clone().run_worker());
            tokio::spawn(state.quarantine_queue.clone().run_worker());
            let app = cleanupstorages::web::build_router_with(state);
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

/// POST JSON with the CSRF token attached, as the real page's `apiPost` does (see
/// `src/web_ui.rs`'s `x-cleanup-token` header) -- `tests/browse_server.rs::http_post_json` never
/// attaches this, because it exists only to prove the *missing*-token case.
fn post_json(addr: std::net::SocketAddr, path: &str, csrf: &str, body: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(
        s,
        "POST {path} HTTP/1.0\r\nHost: localhost\r\nContent-Type: application/json\r\nx-cleanup-token: {csrf}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    buf
}

/// Pull the per-run CSRF token out of the overview page's `<meta name="csrf" content="...">`, the
/// same place the real front-end JS reads it from (`src/web_ui.rs`).
fn csrf_token(addr: std::net::SocketAddr) -> String {
    let body = http_get(addr, "/");
    let marker = "name=\"csrf\" content=\"";
    let start = body.find(marker).expect("csrf meta tag present") + marker.len();
    let end = body[start..].find('"').unwrap();
    body[start..start + end].to_string()
}

/// The single volume the scan just wrote, read straight from the catalogue -- no CLI output to
/// parse, and the catalogue is the actual source of truth `/api/extract` itself reads from.
fn volume_id_of(data_dir: &std::path::Path) -> String {
    let cat =
        cleanupstorages::catalog::Catalog::open_readonly(&data_dir.join("catalog.db")).unwrap();
    let stats = cat.volume_stats().unwrap();
    assert_eq!(stats.len(), 1, "exactly one volume expected: {stats:?}");
    stats[0].0.clone()
}

/// Wait for an observable end state, not for the queue to *look* idle: the queue reports empty in
/// the window between finishing one job and enqueueing the nested archive it produced. Commit
/// 940750a fixed exactly this race in the quarantine tests -- do not reintroduce it here.
fn wait_for(path: &std::path::Path) {
    for _ in 0..600 {
        if path.exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("timed out waiting for {}", path.display());
}

#[test]
fn extract_unlocks_the_content_recurses_and_quarantines_each_archive() {
    let tmp = tempfile::tempdir().unwrap();
    let drive = tmp.path().join("drive");
    std::fs::create_dir_all(drive.join("sub")).unwrap();

    // inner.zip goes INSIDE bundle.zip, so the catalogue holds a two-segment chain.
    let inner = tmp.path().join("inner.zip");
    write_zip(&inner, &[("deep.txt", b"DEEP")]);
    let inner_bytes = std::fs::read(&inner).unwrap();
    write_zip(
        &drive.join("bundle.zip"),
        &[("a.txt", b"AAA"), ("sub/inner.zip", &inner_bytes)],
    );
    // A loose twin, so the unlocked content is visibly a duplicate afterwards.
    std::fs::write(drive.join("loose_a.txt"), b"AAA").unwrap();

    let data = tmp.path().join("appdata");
    let scan = bin()
        .env("CLEANUPSTORAGES_DATA_DIR", &data)
        .args(["scan"])
        .arg(&drive)
        .args(["--readonly-fallback", "fingerprint"])
        .output()
        .unwrap();
    assert!(
        scan.status.success(),
        "scan failed: {}",
        String::from_utf8_lossy(&scan.stderr)
    );

    // Start the server against the same data dir and POST the extraction.
    let addr = start_server_for(&data);
    let csrf = csrf_token(addr);
    let volume_id = volume_id_of(&data);
    let resp = post_json(
        addr,
        "/api/extract",
        &csrf,
        &format!(r#"{{"volume_id":"{volume_id}","paths":["bundle.zip"]}}"#),
    );
    assert!(resp.contains("200 OK"), "extract request accepted: {resp}");

    // The outer archive.
    wait_for(&drive.join("bundle/a.txt"));
    assert_eq!(std::fs::read(drive.join("bundle/a.txt")).unwrap(), b"AAA");
    assert!(
        drive.join("_ToDelete/bundle.zip").is_file(),
        "the original is quarantined, never deleted"
    );
    assert!(
        !drive.join("bundle.zip").is_file(),
        "original moved out of the way"
    );

    // The nested one, enqueued by the first job and extracted by the same worker.
    wait_for(&drive.join("bundle/sub/inner/deep.txt"));
    assert_eq!(
        std::fs::read(drive.join("bundle/sub/inner/deep.txt")).unwrap(),
        b"DEEP"
    );
    assert!(drive.join("_ToDelete/bundle/sub/inner.zip").is_file());

    // The catalogue now describes loose files, not archive entries.
    let search = bin()
        .env("CLEANUPSTORAGES_DATA_DIR", &data)
        .args(["search", "a.txt"])
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&search.stdout).to_string();
    assert!(
        out.contains("bundle/a.txt"),
        "extracted file is catalogued loose: {out}"
    );
    assert!(
        !out.contains("bundle.zip \u{203a}"),
        "no row may still point inside the archive that just moved: {out}"
    );
}
