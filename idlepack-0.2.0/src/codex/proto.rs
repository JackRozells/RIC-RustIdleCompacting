//! Reading the Codex app-server's JSON-RPC stream, one line at a time.
//!
//! Only a handful of messages matter, and most lines are not them, so each
//! line is first screened with a substring test and parsed only when it might
//! be one. Everything here is pure: given a line, say what it means.
//!
//! The shapes are the app-server protocol's v2 (codex-rs/app-server-protocol):
//! `turn/started` and `turn/completed` carry `{ threadId, turn }`;
//! `thread/tokenUsage/updated` carries `{ threadId, turnId, tokenUsage: { total,
//! last, modelContextWindow } }`, where `last.totalTokens` is the context of the
//! thread's latest call; `item/completed` carries `{ threadId, item }`, and a
//! compaction is an item of type `contextCompaction`.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum ServerEvent {
    TurnStarted {
        thread: String,
    },
    TurnCompleted {
        thread: String,
        status: String,
        compacted: bool,
    },
    TokenUsage {
        thread: String,
        context: Option<i64>,
        window: Option<i64>,
        estimated_only: bool,
    },
    Compacted {
        thread: String,
    },
    /// A reply to a request whose id carries our tag.
    OurReply {
        id: String,
        error: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    /// The person started a turn (or asked for anything that runs the model).
    Activity { thread: String },
    /// The person compacted the thread themselves.
    CompactRequested { thread: String },
}

fn thread_of(params: &Value) -> Option<String> {
    for key in ["threadId", "thread_id", "conversationId"] {
        if let Some(id) = params.get(key).and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
            return Some(id.to_owned());
        }
    }
    let thread = params.get("thread")?;
    for key in ["id", "threadId"] {
        if let Some(id) = thread.get(key).and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
            return Some(id.to_owned());
        }
    }
    None
}

fn item_is_compaction(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("contextCompaction")
}

/// What one line from the app-server means to us, if anything.
pub fn server_event(line: &[u8], tag: &str) -> Option<ServerEvent> {
    let text = std::str::from_utf8(line).ok()?;
    let interesting = text.contains(tag)
        || text.contains("\"turn/started\"")
        || text.contains("\"turn/completed\"")
        || text.contains("\"thread/tokenUsage/updated\"")
        || text.contains("\"thread/compacted\"")
        || (text.contains("\"item/completed\"") && text.contains("contextCompaction"));
    if !interesting {
        return None;
    }
    let message: Value = serde_json::from_str(text.trim_end()).ok()?;
    if let Some(id) = message.get("id").and_then(Value::as_str) {
        if id.starts_with(tag) && message.get("method").is_none() {
            let error = message
                .get("error")
                .map(|e| e.get("message").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| e.to_string()));
            return Some(ServerEvent::OurReply { id: id.to_owned(), error });
        }
    }
    // A message with an id is somebody's request or reply, never a notification.
    if message.get("id").is_some() {
        return None;
    }
    let params = message.get("params")?;
    match message.get("method")?.as_str()? {
        "turn/started" => Some(ServerEvent::TurnStarted { thread: thread_of(params)? }),
        "turn/completed" => {
            let turn = params.get("turn");
            let status = turn.and_then(|t| t.get("status")).and_then(Value::as_str).unwrap_or("completed").to_owned();
            let compacted = turn
                .and_then(|t| t.get("items"))
                .and_then(Value::as_array)
                .map(|items| items.iter().any(item_is_compaction))
                .unwrap_or(false);
            Some(ServerEvent::TurnCompleted { thread: thread_of(params)?, status, compacted })
        }
        "thread/tokenUsage/updated" => {
            let usage = params.get("tokenUsage")?;
            let last = usage.get("last");
            let field = |k: &str| last.and_then(|l| l.get(k)).and_then(Value::as_i64);
            let context = field("totalTokens").filter(|n| *n > 0);
            // After a compaction Codex re-estimates the history and reports it with
            // no input or output of its own: that is the new context, not a call.
            let estimated_only = field("inputTokens") == Some(0) && field("outputTokens") == Some(0);
            let window = usage.get("modelContextWindow").and_then(Value::as_i64).filter(|n| *n > 0);
            Some(ServerEvent::TokenUsage { thread: thread_of(params)?, context, window, estimated_only })
        }
        "thread/compacted" => Some(ServerEvent::Compacted { thread: thread_of(params)? }),
        "item/completed" => {
            let item = params.get("item")?;
            if item_is_compaction(item) {
                Some(ServerEvent::Compacted { thread: thread_of(params)? })
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Requests from the desktop app that mean somebody is using a thread.
const ACTIVITY: &[&str] = &["turn/start", "turn/steer", "review/start", "thread/shellCommand"];

/// What one line from the desktop app means to us, if anything.
pub fn client_event(line: &[u8]) -> Option<ClientEvent> {
    let text = std::str::from_utf8(line).ok()?;
    let interesting =
        ACTIVITY.iter().any(|m| text.contains(&format!("\"{m}\""))) || text.contains("\"thread/compact/start\"");
    if !interesting {
        return None;
    }
    let message: Value = serde_json::from_str(text.trim_end()).ok()?;
    let method = message.get("method")?.as_str()?;
    let thread = thread_of(message.get("params")?)?;
    if method == "thread/compact/start" {
        Some(ClientEvent::CompactRequested { thread })
    } else if ACTIVITY.contains(&method) {
        Some(ClientEvent::Activity { thread })
    } else {
        None
    }
}

/// The request we write into the app-server's stdin to compact a thread.
pub fn compact_request(id: &str, thread: &str) -> Vec<u8> {
    let mut line = serde_json::to_vec(&serde_json::json!({
        "id": id,
        "method": "thread/compact/start",
        "params": { "threadId": thread },
    }))
    .expect("serializable");
    line.push(b'\n');
    line
}

/// Whether these arguments start the stdio app-server a desktop client talks
/// to: `codex [global flags] app-server [flags]` with no tooling subcommand
/// and no socket or WebSocket listener.
pub fn is_stdio_app_server(args: &[String]) -> bool {
    const SUBCOMMANDS: &[&str] = &[
        "daemon",
        "proxy",
        "generate-ts",
        "generate-json-schema",
        "generate-internal-json-schema",
        "bootstrap",
        "start",
        "restart",
        "stop",
        "update",
        "status",
        "help",
    ];
    let Some(at) = args.iter().position(|a| a == "app-server") else { return false };
    let mut rest = args[at + 1..].iter();
    while let Some(arg) = rest.next() {
        if SUBCOMMANDS.contains(&arg.as_str()) {
            return false;
        }
        if arg == "--listen" {
            match rest.next() {
                Some(url) if url.starts_with("stdio") => continue,
                _ => return false,
            }
        }
        if let Some(url) = arg.strip_prefix("--listen=") {
            if !url.starts_with("stdio") {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAG: &str = "idlepack-42-";

    fn line(v: serde_json::Value) -> Vec<u8> {
        let mut l = serde_json::to_vec(&v).unwrap();
        l.push(b'\n');
        l
    }

    #[test]
    fn reads_turn_lifecycle() {
        let started = line(
            serde_json::json!({"method":"turn/started","params":{"threadId":"t1","turn":{"id":"u1","items":[],"status":"inProgress"}}}),
        );
        assert_eq!(server_event(&started, TAG), Some(ServerEvent::TurnStarted { thread: "t1".into() }));
        let done = line(
            serde_json::json!({"method":"turn/completed","params":{"threadId":"t1","turn":{"id":"u1","items":[],"status":"completed"}}}),
        );
        assert_eq!(
            server_event(&done, TAG),
            Some(ServerEvent::TurnCompleted { thread: "t1".into(), status: "completed".into(), compacted: false })
        );
        let compaction_turn = line(
            serde_json::json!({"method":"turn/completed","params":{"threadId":"t1","turn":{"id":"u2","items":[{"type":"contextCompaction","id":"i"}],"status":"completed"}}}),
        );
        assert_eq!(
            server_event(&compaction_turn, TAG),
            Some(ServerEvent::TurnCompleted { thread: "t1".into(), status: "completed".into(), compacted: true })
        );
    }

    #[test]
    fn reads_token_usage_and_compaction_items() {
        let usage = line(
            serde_json::json!({"method":"thread/tokenUsage/updated","params":{"threadId":"t1","turnId":"u1","tokenUsage":{
            "total":{"totalTokens":9,"inputTokens":9,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":0,"reasoningOutputTokens":0},
            "last":{"totalTokens":312000,"inputTokens":310000,"cachedInputTokens":300000,"cacheWriteInputTokens":0,"outputTokens":2000,"reasoningOutputTokens":0},
            "modelContextWindow":400000}}}),
        );
        assert_eq!(
            server_event(&usage, TAG),
            Some(ServerEvent::TokenUsage {
                thread: "t1".into(),
                context: Some(312_000),
                window: Some(400_000),
                estimated_only: false
            })
        );
        let estimate = line(
            serde_json::json!({"method":"thread/tokenUsage/updated","params":{"threadId":"t1","turnId":"u2","tokenUsage":{
            "total":{"totalTokens":1},"last":{"totalTokens":18000,"inputTokens":0,"outputTokens":0},"modelContextWindow":null}}}),
        );
        assert_eq!(
            server_event(&estimate, TAG),
            Some(ServerEvent::TokenUsage {
                thread: "t1".into(),
                context: Some(18_000),
                window: None,
                estimated_only: true
            })
        );
        let item = line(
            serde_json::json!({"method":"item/completed","params":{"threadId":"t1","turnId":"u2","item":{"type":"contextCompaction","id":"x"},"completedAtMs":1}}),
        );
        assert_eq!(server_event(&item, TAG), Some(ServerEvent::Compacted { thread: "t1".into() }));
        let other = line(
            serde_json::json!({"method":"item/completed","params":{"threadId":"t1","item":{"type":"agentMessage","text":"contextCompaction"}}}),
        );
        assert_eq!(server_event(&other, TAG), None);
    }

    #[test]
    fn lifts_only_our_replies() {
        let ours = line(serde_json::json!({"id":"idlepack-42-7","result":{}}));
        assert_eq!(server_event(&ours, TAG), Some(ServerEvent::OurReply { id: "idlepack-42-7".into(), error: None }));
        let refused =
            line(serde_json::json!({"id":"idlepack-42-8","error":{"code":-32600,"message":"thread not found: t9"}}));
        assert_eq!(
            server_event(&refused, TAG),
            Some(ServerEvent::OurReply { id: "idlepack-42-8".into(), error: Some("thread not found: t9".into()) })
        );
        let theirs = line(serde_json::json!({"id":"7","result":{"note":"idlepack-42-"}}));
        assert_eq!(server_event(&theirs, TAG), None);
        let numeric = line(serde_json::json!({"id":7,"result":{}}));
        assert_eq!(server_event(&numeric, TAG), None);
    }

    #[test]
    fn ignores_everything_else_cheaply() {
        assert_eq!(
            server_event(b"{\"method\":\"item/agentMessage/delta\",\"params\":{\"delta\":\"hi\"}}\n", TAG),
            None
        );
        assert_eq!(server_event(b"not json at all\n", TAG), None);
        assert_eq!(server_event(&[0xff, 0xfe, b'\n'], TAG), None);
    }

    #[test]
    fn reads_the_desktop_side() {
        let start = line(serde_json::json!({"id":3,"method":"turn/start","params":{"threadId":"t1","input":[]}}));
        assert_eq!(client_event(&start), Some(ClientEvent::Activity { thread: "t1".into() }));
        let compact = line(serde_json::json!({"id":4,"method":"thread/compact/start","params":{"threadId":"t1"}}));
        assert_eq!(client_event(&compact), Some(ClientEvent::CompactRequested { thread: "t1".into() }));
        let read = line(serde_json::json!({"id":5,"method":"thread/read","params":{"threadId":"t1"}}));
        assert_eq!(client_event(&read), None);
    }

    #[test]
    fn our_request_is_one_line() {
        let req = compact_request("idlepack-42-1", "t1");
        assert_eq!(req.iter().filter(|b| **b == b'\n').count(), 1);
        let v: Value = serde_json::from_slice(&req).unwrap();
        assert_eq!(v["method"], "thread/compact/start");
        assert_eq!(v["params"]["threadId"], "t1");
    }

    #[test]
    fn recognises_the_stdio_app_server() {
        let a = |s: &str| s.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
        assert!(is_stdio_app_server(&a("app-server")));
        assert!(is_stdio_app_server(&a("-c model=x app-server --stdio")));
        assert!(is_stdio_app_server(&a("app-server --listen stdio://")));
        assert!(is_stdio_app_server(&a("app-server --listen=stdio://")));
        assert!(!is_stdio_app_server(&a("app-server --listen ws://127.0.0.1:9000")));
        assert!(!is_stdio_app_server(&a("app-server proxy")));
        assert!(!is_stdio_app_server(&a("app-server generate-ts --out x")));
        assert!(!is_stdio_app_server(&a("exec hello")));
        assert!(!is_stdio_app_server(&a("--version")));
    }
}
