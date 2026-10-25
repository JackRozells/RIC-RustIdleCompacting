//! A stand-in for `codex app-server` (the same script as tests/fixtures/fake_codex.py,
//! in Rust so it runs where Python does not, such as a Windows test box):
//! answers `initialize`, plays a turn for `turn/start`, and a compaction turn for
//! `thread/compact/start`. Each request it gets is appended to $FAKE_CODEX_LOG.
//!
//!   cargo build --example fake_app_server
//!   IDLEPACK_CODEX_REAL=target/debug/examples/fake_app_server idlepack __codex-shim app-server

use serde_json::{json, Value};
use std::io::{BufRead, Write};

fn send(out: &mut impl Write, v: Value) {
    let _ = writeln!(out, "{}", v);
    let _ = out.flush();
}

fn usage(thread: &str, turn: &str, total: i64, input: i64, output: i64) -> Value {
    let block = json!({ "totalTokens": total, "inputTokens": input, "cachedInputTokens": 0, "cacheWriteInputTokens": 0,
                        "outputTokens": output, "reasoningOutputTokens": 0 });
    json!({ "method": "thread/tokenUsage/updated", "params": { "threadId": thread, "turnId": turn,
            "tokenUsage": { "total": block, "last": block, "modelContextWindow": 400000 } } })
}

fn main() {
    if std::env::args().any(|a| a == "--version") {
        println!("codex-fake 1.0 --version");
        std::process::exit(3);
    }
    let log = std::env::var_os("FAKE_CODEX_LOG");
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    let mut turns = 0;
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        if let Some(path) = &log {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(f, "{msg}");
            }
        }
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let thread = msg.pointer("/params/threadId").and_then(Value::as_str).unwrap_or("").to_owned();
        match method {
            "initialize" => send(&mut out, json!({ "id": id, "result": { "userAgent": "fake" } })),
            "turn/start" => {
                turns += 1;
                let turn = format!("u{turns}");
                let context = msg.pointer("/params/context").and_then(Value::as_i64).unwrap_or(312_000);
                send(
                    &mut out,
                    json!({ "id": id, "result": { "turn": { "id": turn, "items": [], "status": "inProgress" } } }),
                );
                send(
                    &mut out,
                    json!({ "method": "turn/started", "params": { "threadId": thread, "turn": { "id": turn, "items": [], "status": "inProgress" } } }),
                );
                send(&mut out, usage(&thread, &turn, context, context - 2_000, 2_000));
                send(
                    &mut out,
                    json!({ "method": "turn/completed", "params": { "threadId": thread, "turn": { "id": turn, "items": [], "status": "completed" } } }),
                );
            }
            "thread/compact/start" => {
                turns += 1;
                let turn = format!("u{turns}");
                let item = json!({ "type": "contextCompaction", "id": "c1" });
                send(&mut out, json!({ "id": id, "result": {} }));
                send(
                    &mut out,
                    json!({ "method": "turn/started", "params": { "threadId": thread, "turn": { "id": turn, "items": [], "status": "inProgress" } } }),
                );
                send(&mut out, usage(&thread, &turn, 18_000, 0, 0));
                send(
                    &mut out,
                    json!({ "method": "item/completed", "params": { "threadId": thread, "turnId": turn, "item": item, "completedAtMs": 2 } }),
                );
                send(
                    &mut out,
                    json!({ "method": "turn/completed", "params": { "threadId": thread, "turn": { "id": turn, "items": [item], "status": "completed" } } }),
                );
            }
            _ if !id.is_null() => send(
                &mut out,
                json!({ "id": id, "error": { "code": -32601, "message": format!("unknown method {method}") } }),
            ),
            _ => {}
        }
    }
}
