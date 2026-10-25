//! The Codex shim end to end, against a fake app-server (tests/fixtures/fake_codex.py):
//! it passes the desktop's traffic through untouched, compacts a big idle thread
//! by writing into the app-server's stdin, keeps its own reply from the desktop,
//! leaves a small thread alone, and records what it did.
#![cfg(unix)]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn python() -> Option<String> {
    for name in ["python3", "python"] {
        if Command::new(name).arg("--version").output().map(|o| o.status.success()).unwrap_or(false) {
            return Some(name.to_owned());
        }
    }
    None
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("idlepack-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The fake app-server as an executable of its own, the way the desktop's codex is one.
fn fake_codex(dir: &Path, python: &str) -> PathBuf {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_codex.py");
    let exe = dir.join("codex-real");
    std::fs::write(&exe, format!("#!/bin/sh\nexec {python} {} \"$@\"\n", script.display())).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    exe
}

fn lines_of(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

#[test]
fn compacts_an_idle_thread_through_the_desktops_own_app_server() {
    let Some(python) = python() else {
        eprintln!("skipped: no python to run the fake app-server");
        return;
    };
    let home = scratch("shim");
    let real = fake_codex(&home, &python);
    let received = home.join("received.jsonl");
    std::fs::write(
        home.join("config.json"),
        serde_json::to_string(&json!({ "codex": { "tickMs": 25, "_testIdleMs": 400 } })).unwrap(),
    )
    .unwrap();

    let mut shim = Command::new(env!("CARGO_BIN_EXE_idlepack"))
        .args(["__codex-shim", "app-server"])
        .env("IDLEPACK_HOME", &home)
        .env("IDLEPACK_CODEX_REAL", &real)
        .env("FAKE_CODEX_LOG", &received)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start the shim");
    let mut desktop = shim.stdin.take().unwrap();
    let out = shim.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel::<Value>();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines() {
            let Ok(line) = line else { break };
            let value: Value = serde_json::from_str(&line).expect("the desktop only ever gets whole JSON lines");
            if tx.send(value).is_err() {
                break;
            }
        }
    });
    let mut seen: Vec<Value> = Vec::new();
    let wait_for = |what: &dyn Fn(&Value) -> bool, seen: &mut Vec<Value>, secs: u64| -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        if seen.iter().any(what) {
            return true;
        }
        while let Ok(v) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            let hit = what(&v);
            seen.push(v);
            if hit {
                return true;
            }
        }
        false
    };

    // Owns the desktop's end of the pipe: dropping it closes the shim's stdin.
    let mut send = move |v: Value| {
        writeln!(desktop, "{}", serde_json::to_string(&v).unwrap()).unwrap();
        desktop.flush().unwrap();
    };
    send(json!({ "id": 1, "method": "initialize", "params": { "clientInfo": { "name": "desktop" } } }));
    assert!(wait_for(&|v| v["id"] == 1, &mut seen, 10), "the desktop's own request is answered through the shim");

    // A big thread and a small one, each finishing a turn.
    send(json!({ "id": 2, "method": "turn/start", "params": { "threadId": "big-thread-1", "input": [] } }));
    send(
        json!({ "id": 3, "method": "turn/start", "params": { "threadId": "small-thread", "input": [], "context": 60000 } }),
    );
    assert!(wait_for(&|v| v["method"] == "turn/completed" && v["params"]["threadId"] == "small-thread", &mut seen, 10));

    // ~400 ms later the big one is compacted: the desktop sees the compaction turn...
    let compacted = |v: &Value| {
        v["method"] == "turn/completed"
            && v["params"]["threadId"] == "big-thread-1"
            && v["params"]["turn"]["items"].as_array().map(|i| !i.is_empty()).unwrap_or(false)
    };
    assert!(wait_for(&compacted, &mut seen, 10), "the idle thread is compacted");
    // ...but never the reply to the request it did not send.
    std::thread::sleep(Duration::from_millis(600));
    while let Ok(v) = rx.try_recv() {
        seen.push(v);
    }
    assert!(
        !seen.iter().any(|v| v["id"].as_str().map(|id| id.starts_with("idlepack-")).unwrap_or(false)),
        "our reply was lifted out of the stream"
    );

    // The app-server got exactly one compaction request, for the big thread, under our id.
    let requests = lines_of(&received);
    let compactions: Vec<&Value> = requests.iter().filter(|r| r["method"] == "thread/compact/start").collect();
    assert_eq!(compactions.len(), 1, "one compaction, not one per tick: {requests:?}");
    assert_eq!(compactions[0]["params"]["threadId"], "big-thread-1");
    assert!(compactions[0]["id"].as_str().unwrap().starts_with("idlepack-"));
    // Everything the desktop sent arrived, in order.
    let theirs: Vec<Value> =
        requests.iter().filter(|r| r["method"] != "thread/compact/start").map(|r| r["id"].clone()).collect();
    assert_eq!(theirs, vec![json!(1), json!(2), json!(3)]);

    // It was recorded, with the sizes from the app-server's own token reports.
    let events: Vec<Value> = std::fs::read_dir(home.join("events"))
        .unwrap()
        .flatten()
        .filter_map(|e| serde_json::from_str(&std::fs::read_to_string(e.path()).unwrap()).ok())
        .collect();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["outcome"], "compacted");
    assert_eq!(events[0]["thread_id"], "big-thread-1");
    assert_eq!(events[0]["tokens_before"], 312000);
    assert_eq!(events[0]["tokens_after"], 18000);

    // The desktop closes its side: the app-server ends and the shim with it.
    drop(send);
    let status = shim.wait().unwrap();
    assert!(status.success(), "the shim exits as the app-server did: {status:?}");
    let log = std::fs::read_to_string(home.join("log/codex-shim.log")).unwrap();
    assert!(log.contains("compacted 312k -> 18k"), "{log}");
    assert!(!log.contains("done"), "the log never holds conversation text");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn anything_but_the_app_server_is_passed_straight_through() {
    let Some(python) = python() else { return };
    let home = scratch("pass");
    let real = fake_codex(&home, &python);
    let out = Command::new(env!("CARGO_BIN_EXE_idlepack"))
        .args(["__codex-shim", "--version"])
        .env("IDLEPACK_HOME", &home)
        .env("IDLEPACK_CODEX_REAL", &real)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "codex-fake 1.0 --version");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn run_under_the_name_codex_it_is_the_shim() {
    let Some(python) = python() else { return };
    let home = scratch("name");
    let real = fake_codex(&home, &python);
    let copy = home.join("codex");
    std::fs::copy(env!("CARGO_BIN_EXE_idlepack"), &copy).unwrap();
    let out = Command::new(&copy)
        .arg("--version")
        .env("IDLEPACK_HOME", &home)
        .env("IDLEPACK_CODEX_REAL", &real)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stdout).contains("codex-fake"));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn without_a_real_codex_it_says_so() {
    let home = scratch("none");
    let out = Command::new(env!("CARGO_BIN_EXE_idlepack"))
        .args(["__codex-shim", "app-server"])
        .env("IDLEPACK_HOME", &home)
        .env("IDLEPACK_CODEX_REAL", home.join("missing"))
        .env("PATH", &home)
        .env("HOME", &home)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(127));
    assert!(String::from_utf8_lossy(&out.stderr).contains("could not find the real codex"));
    let _ = std::fs::remove_dir_all(&home);
}
