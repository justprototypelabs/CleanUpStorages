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
    // quarantine::quarantine_files() creates the `_ToDelete` directory microseconds before the
    // rename lands the file inside it, so polling on the directory's existence can break in that
    // gap and race the assertions below. Poll on the effect the test actually asserts instead --
    // that exactly one of the two original copies is still on disk -- which can only make the
    // wait longer, never let it pass early.
    let remaining_now = || {
        [
            drive.join("a.jpg").exists(),
            drive.join("copy/a.jpg").exists(),
        ]
        .iter()
        .filter(|x| **x)
        .count()
    };
    let mut remaining = remaining_now();
    for _ in 0..400 {
        remaining = remaining_now();
        if remaining == 1 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }

    // exactly one copy remains on disk, the other is in _ToDelete
    assert_eq!(remaining, 1);
    assert!(drive.join("_ToDelete").exists());
}

/// Seed a drive with two duplicate groups that both span `dirA` and `dirB` -- the partial-overlap
/// shape identical-tree collapse cannot reach, since `dirA` also holds a file `dirB` does not.
fn seed_cluster_drive() -> (std::path::PathBuf, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("c.db");
    let drive = tmp.path().join("driveC");
    std::fs::create_dir_all(drive.join("dirA")).unwrap();
    std::fs::create_dir_all(drive.join("dirB")).unwrap();
    std::fs::write(drive.join(".cleanupstorages_id"), "vol-1").unwrap();
    std::fs::write(drive.join("dirA/a.txt"), b"SAME-CONTENT").unwrap();
    std::fs::write(drive.join("dirB/a.txt"), b"SAME-CONTENT").unwrap();
    std::fs::write(drive.join("dirA/b.txt"), b"OTHER-CONTENT").unwrap();
    std::fs::write(drive.join("dirB/b.txt"), b"OTHER-CONTENT").unwrap();
    // Only in dirA: this is what makes the two folders overlap rather than match.
    std::fs::write(drive.join("dirA/only-here.txt"), b"UNIQUE").unwrap();
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
    (db, drive)
}

fn json_body(raw: &str) -> serde_json::Value {
    let body = raw.split("\r\n\r\n").nth(1).unwrap_or("");
    serde_json::from_str(body.trim()).unwrap()
}

fn post_json(addr: std::net::SocketAddr, path: &str, payload: &str) -> String {
    req(
        addr,
        &format!(
            "POST {path} HTTP/1.0\r\nHost: x\r\ncontent-type: application/json\r\nx-cleanup-token: TESTTOKEN\r\ncontent-length: {}\r\n\r\n{}",
            payload.len(),
            payload
        ),
    )
}

/// One ranking of two folders quarantines every redundant copy and keeps every preferred one.
#[test]
fn confirming_a_cluster_quarantines_the_victims_and_leaves_the_keepers() {
    let (db, drive) = seed_cluster_drive();
    let addr = start(db, drive.clone());

    // min_size=0: the fixture's files are a few bytes, well under the 1 MiB review floor.
    let list = req(
        addr,
        "GET /api/duplicate-clusters?min_size=0&limit=10&offset=0 HTTP/1.0\r\nHost: x\r\n\r\n",
    );
    assert!(list.contains("200 OK"), "list: {list}");
    let list = json_body(&list);
    assert_eq!(list["total"], 1, "both groups share the same folder pair");
    let c = &list["clusters"][0];
    assert_eq!(c["group_count"], 2);
    assert_eq!(c["keepable"], true);
    let cluster_id = c["id"].as_str().unwrap().to_string();

    let payload = serde_json::json!({
        "cluster_id": cluster_id,
        "min_size": 0,
        "preference": [{"volume_id": "vol-1", "dir": "dirA"}],
    })
    .to_string();
    let resp = post_json(addr, "/api/quarantine-cluster", &payload);
    assert!(resp.contains("200 OK"), "confirm: {resp}");
    let resp = json_body(&resp);
    assert_eq!(resp["queued"], 2, "the two dirB copies");
    assert_eq!(resp["skipped"], 0);

    // The request only enqueued; poll on the effect this test asserts rather than on `_ToDelete`,
    // which exists microseconds before the first rename lands inside it.
    let victims_left = || {
        [
            drive.join("dirB/a.txt").exists(),
            drive.join("dirB/b.txt").exists(),
        ]
        .iter()
        .filter(|x| **x)
        .count()
    };
    let mut left = victims_left();
    for _ in 0..400 {
        left = victims_left();
        if left == 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert_eq!(left, 0, "every redundant copy moved");
    assert!(drive.join("dirA/a.txt").exists(), "keepers stay put");
    assert!(drive.join("dirA/b.txt").exists(), "keepers stay put");
    assert!(drive.join("dirA/only-here.txt").exists());
    assert!(drive.join("_ToDelete").exists());
}

/// A cluster id from a list loaded before the catalogue changed is refused, never reapplied to a
/// membership the user did not see.
#[test]
fn a_stale_cluster_confirm_is_refused_and_moves_nothing() {
    let (db, drive) = seed_cluster_drive();
    let addr = start(db.clone(), drive.clone());

    let list = json_body(&req(
        addr,
        "GET /api/duplicate-clusters?min_size=0&limit=10&offset=0 HTTP/1.0\r\nHost: x\r\n\r\n",
    ));
    let cluster_id = list["clusters"][0]["id"].as_str().unwrap().to_string();

    // The catalogue moves on: something else already quarantined the dirB rows, so the {dirA,dirB}
    // set no longer describes any group.
    {
        let cat = cleanupstorages::catalog::Catalog::open(&db).unwrap();
        cat.conn
            .execute(
                "UPDATE files SET status='quarantined' WHERE relative_path LIKE 'dirB%'",
                [],
            )
            .unwrap();
    }

    let payload = serde_json::json!({
        "cluster_id": cluster_id,
        "min_size": 0,
        "preference": [{"volume_id": "vol-1", "dir": "dirA"}],
    })
    .to_string();
    let resp = post_json(addr, "/api/quarantine-cluster", &payload);
    assert!(
        resp.contains("409"),
        "stale confirm must be refused: {resp}"
    );
    assert!(drive.join("dirA/a.txt").exists());
    assert!(drive.join("dirB/a.txt").exists(), "nothing was applied");
}

/// A directory that is not part of the cluster cannot be ranked into it.
#[test]
fn a_preference_naming_an_unknown_directory_is_a_bad_request() {
    let (db, drive) = seed_cluster_drive();
    let addr = start(db, drive.clone());

    let list = json_body(&req(
        addr,
        "GET /api/duplicate-clusters?min_size=0&limit=10&offset=0 HTTP/1.0\r\nHost: x\r\n\r\n",
    ));
    let cluster_id = list["clusters"][0]["id"].as_str().unwrap().to_string();

    let payload = serde_json::json!({
        "cluster_id": cluster_id,
        "min_size": 0,
        "preference": [{"volume_id": "vol-1", "dir": "dirZ"}],
    })
    .to_string();
    let resp = post_json(addr, "/api/quarantine-cluster", &payload);
    assert!(resp.contains("400"), "unknown directory: {resp}");
    assert!(drive.join("dirB/a.txt").exists(), "nothing was applied");
}
