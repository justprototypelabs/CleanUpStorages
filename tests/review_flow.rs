use std::io::{Read, Write};
use std::net::TcpStream;

fn start(state_db: std::path::PathBuf, drive: std::path::PathBuf) -> std::net::SocketAddr {
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let mut mounts = std::collections::HashMap::new();
            mounts.insert("vol-1".to_string(), drive);
            let state = cleanupstorages::web::AppState {
                catalog_path: state_db.clone(),
                mounts: cleanupstorages::mounts::MountResolver::Fixed(mounts.clone()),
                csrf_token: "TESTTOKEN".to_string(),
                scan_queue: cleanupstorages::scan_queue::ScanQueue::new(state_db.clone()),
                // Same resolver the rest of the state uses -- an empty one would make every
                // queued job fail with "drive not connected" while the request path looked fine.
                quarantine_queue: cleanupstorages::quarantine_queue::QuarantineQueue::new(
                    state_db,
                    cleanupstorages::mounts::MountResolver::Fixed(mounts),
                ),
            };
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

/// Limits for tests: the compiled-in defaults, with NO ambient environment read.
fn test_limits() -> cleanupstorages::archive::ArchiveLimits {
    cleanupstorages::archive::ArchiveLimits {
        max_depth: 8,
        buffer_max_bytes: 2 * 1024 * 1024 * 1024,
        total_buffer_bytes: 2 * 1024 * 1024 * 1024,
        entry_max_bytes: Some(64 * 1024 * 1024 * 1024),
        ratio_cap: 10_000,
        deny_extensions: Vec::new(),
        allow_extensions: Vec::new(),
    }
}

fn req(addr: std::net::SocketAddr, raw: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.write_all(raw.as_bytes()).unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    buf
}

#[test]
fn review_duplicates_then_quarantine_over_http() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("c.db");
    let drive = tmp.path().join("driveA");
    std::fs::create_dir_all(drive.join("copy")).unwrap();
    std::fs::write(drive.join(".cleanupstorages_id"), "vol-1").unwrap();
    std::fs::write(drive.join("a.jpg"), b"DUP").unwrap();
    std::fs::write(drive.join("copy/a.jpg"), b"DUP").unwrap();
    {
        let cat = cleanupstorages::catalog::Catalog::open(&db).unwrap();
        cat.upsert_volume(&cleanupstorages::catalog::models::Volume {
            volume_id: "vol-1".into(),
            label: "D".into(),
            identified_by: "marker".into(),
            first_seen_at: 1,
            last_seen_at: 1,
        })
        .unwrap();
        let ident = cleanupstorages::volume::VolumeIdentity {
            volume_id: "vol-1".into(),
            label: "D".into(),
            identified_by: "marker".into(),
        };
        cleanupstorages::scanner::scan_volume(&cat, &drive, &ident, false, 100, &test_limits())
            .unwrap();
    }
    std::mem::forget(tmp);

    let addr = start(db.clone(), drive.clone());

    // 1) fetch duplicates, grab a victim id (a copy that is NOT the suggested keep)
    // min_size=0: the fixture's files are a few bytes, well under the 1 MiB review floor.
    let dups = req(
        addr,
        "GET /api/duplicates?min_size=0 HTTP/1.0\r\nHost: x\r\n\r\n",
    );
    assert!(dups.contains("200 OK"), "dups: {dups}");
    let body = dups.split("\r\n\r\n").nth(1).unwrap_or("");
    let json: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
    let g = &json["groups"][0];
    let keep = g["suggested_keep_id"].as_i64().unwrap();
    let victim = g["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"].as_i64() != Some(keep))
        .unwrap()["id"]
        .as_i64()
        .unwrap();

    // 2) POST quarantine with the token
    let payload = format!("{{\"quarantine_ids\":[{victim}]}}");
    let post = format!("POST /api/quarantine HTTP/1.0\r\nHost: x\r\ncontent-type: application/json\r\nx-cleanup-token: TESTTOKEN\r\ncontent-length: {}\r\n\r\n{}", payload.len(), payload);
    let resp = req(addr, &post);
    assert!(resp.contains("200 OK"), "quarantine resp: {resp}");
    assert!(resp.contains("\"queued\":1"), "resp: {resp}");

    // 3) the request only enqueued the job; poll for the worker to actually move the file.
    for _ in 0..400 {
        if drive.join("_ToDelete").exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }

    // exactly one copy remains on disk, the other is in _ToDelete
    let remaining = [
        drive.join("a.jpg").exists(),
        drive.join("copy/a.jpg").exists(),
    ]
    .iter()
    .filter(|x| **x)
    .count();
    assert_eq!(remaining, 1);
    assert!(drive.join("_ToDelete").exists());
}
