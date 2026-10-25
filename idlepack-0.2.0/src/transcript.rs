//! What a Claude Code transcript says about a session's prompt cache.
//!
//! Each main-thread assistant entry in `~/.claude/projects/<project>/<id>.jsonl`
//! carries the API usage of the call that produced it, including how many
//! tokens it wrote to the cache and under which lifetime
//! (`cache_creation.ephemeral_5m_input_tokens` / `ephemeral_1h_input_tokens`).
//! The newest such entry gives the time of the last request and the context it
//! saw; the newest one that wrote to the cache gives the lifetime in force.
//! The file is read backwards, in windows that grow, so a transcript of
//! hundreds of megabytes costs a few reads.

use crate::timeutil;
use serde_json::Value;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const FIRST_WINDOW: u64 = 256 * 1024;
const MAX_SCAN_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 20_000;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct CacheFacts {
    pub last_call_ms: Option<i64>,
    pub context_tokens: Option<i64>,
    pub ttl_s: Option<i64>,
    pub model: Option<String>,
    /// The text of the newest main-thread assistant entry, to tell whether the
    /// turn the Stop hook is about has reached the file yet.
    pub last_text: Option<String>,
}

fn entry_text(entry: &Value) -> Option<String> {
    let content = entry.get("message")?.get("content")?;
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }
    let parts: Vec<&str> = content
        .as_array()?
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// The context one entry implies, if it is a main-thread assistant message with usage.
pub fn usage_tokens(entry: &Value) -> Option<i64> {
    if entry.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    if entry.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let message = entry.get("message")?;
    if message.get("model").and_then(Value::as_str) == Some("<synthetic>") {
        return None;
    }
    let usage = message.get("usage")?;
    let total: i64 = ["input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens", "output_tokens"]
        .iter()
        .filter_map(|k| usage.get(*k).and_then(Value::as_i64))
        .sum();
    (total > 0).then_some(total)
}

/// The cache lifetime an entry's call wrote under, if it wrote anything. A call
/// that wrote any 5-minute part counts as 5 minutes: that part expires first.
pub fn entry_ttl_s(entry: &Value) -> Option<i64> {
    usage_tokens(entry)?;
    let breakdown = entry.get("message")?.get("usage")?.get("cache_creation")?;
    let count = |key: &str| breakdown.get(key).and_then(Value::as_i64).unwrap_or(0);
    if count("ephemeral_5m_input_tokens") > 0 {
        Some(300)
    } else if count("ephemeral_1h_input_tokens") > 0 {
        Some(3_600)
    } else {
        None
    }
}

/// Entries newest first. A line cut by a window's start is read whole in the next.
pub fn entries_from_end(path: &Path) -> impl Iterator<Item = Value> {
    let mut file = File::open(path).ok();
    let mut end = file.as_ref().and_then(|f| f.metadata().ok()).map(|m| m.len()).unwrap_or(0);
    let mut window = FIRST_WINDOW;
    let mut scanned = 0u64;
    let mut pending: Vec<Value> = Vec::new();
    std::iter::from_fn(move || loop {
        if let Some(entry) = pending.pop() {
            return Some(entry);
        }
        let f = file.as_mut()?;
        if end == 0 || scanned >= MAX_SCAN_BYTES {
            return None;
        }
        let start = end.saturating_sub(window);
        let mut buf = vec![0u8; (end - start) as usize];
        if f.seek(SeekFrom::Start(start)).is_err() || f.read_exact(&mut buf).is_err() {
            file = None;
            return None;
        }
        scanned += end - start;
        let mut lines: Vec<&[u8]> = buf.split(|b| *b == b'\n').collect();
        if start > 0 {
            // The first piece may be the tail of a longer line: leave it for a wider read.
            let first = lines.remove(0);
            end = start + first.len() as u64;
            window = window.saturating_mul(4);
        } else {
            end = 0;
        }
        // Parsed oldest first and popped from the back: newest first out.
        pending = lines
            .into_iter()
            .filter(|l| !l.iter().all(u8::is_ascii_whitespace))
            .filter_map(|l| serde_json::from_slice::<Value>(l).ok())
            .filter(Value::is_object)
            .collect();
    })
}

pub fn read_facts(path: &Path) -> CacheFacts {
    let mut facts = CacheFacts::default();
    for entry in entries_from_end(path).take(MAX_ENTRIES) {
        if usage_tokens(&entry).is_none() {
            continue;
        }
        if facts.context_tokens.is_none() {
            facts.context_tokens = usage_tokens(&entry);
            facts.last_call_ms = entry.get("timestamp").and_then(Value::as_str).and_then(timeutil::parse_rfc3339_ms);
            facts.model = entry.get("message").and_then(|m| m.get("model")).and_then(Value::as_str).map(str::to_owned);
            facts.last_text = entry_text(&entry);
        }
        if let Some(ttl) = entry_ttl_s(&entry) {
            facts.ttl_s = Some(ttl);
            break;
        }
    }
    facts
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn assistant(ts: &str, input: i64, read: i64, write5m: i64, write1h: i64, sidechain: bool) -> Value {
        json!({
            "type": "assistant", "isSidechain": sidechain, "timestamp": ts,
            "message": { "model": "claude-opus-5-5", "content": [ { "type": "text", "text": format!("reply at {ts}") } ], "usage": {
                "input_tokens": input, "cache_read_input_tokens": read,
                "cache_creation_input_tokens": write5m + write1h, "output_tokens": 100,
                "cache_creation": { "ephemeral_5m_input_tokens": write5m, "ephemeral_1h_input_tokens": write1h }
            }}
        })
    }

    fn transcript(lines: &[Value], padding: usize) -> tempfile_lite::TempPath {
        let path = tempfile_lite::path("transcript.jsonl");
        let mut f = File::create(&path.0).unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
            // Long user rows between them, so the windows have to grow.
            if padding > 0 {
                writeln!(f, "{}", json!({ "type": "user", "message": { "content": "x".repeat(padding) } })).unwrap();
            }
        }
        path
    }

    #[test]
    fn newest_call_and_its_lifetime() {
        let t = transcript(
            &[
                assistant("2026-10-23T15:00:00Z", 10, 100_000, 0, 5_000, false),
                assistant("2026-10-23T15:04:05.123Z", 5, 400_000, 0, 0, false),
                assistant("2026-10-23T15:05:00Z", 1, 1, 0, 9, true), // a subagent's: ignored
            ],
            0,
        );
        let facts = read_facts(&t.0);
        assert_eq!(facts.last_call_ms, Some(1_792_767_845_123));
        assert_eq!(facts.context_tokens, Some(400_105));
        assert_eq!(facts.ttl_s, Some(3_600));
        assert_eq!(facts.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(facts.last_text.as_deref(), Some("reply at 2026-10-23T15:04:05.123Z"));
    }

    #[test]
    fn five_minute_parts_win() {
        let t = transcript(&[assistant("2026-10-23T15:00:00Z", 10, 0, 200, 5_000, false)], 0);
        assert_eq!(read_facts(&t.0).ttl_s, Some(300));
    }

    #[test]
    fn reads_across_growing_windows() {
        // Each padding row is ~400 KB, past the first 256 KB window.
        let t = transcript(
            &[
                assistant("2026-10-23T14:00:00Z", 10, 0, 0, 7_000, false),
                assistant("2026-10-23T15:00:00Z", 10, 300_000, 0, 0, false),
            ],
            400_000,
        );
        let facts = read_facts(&t.0);
        assert_eq!(facts.context_tokens, Some(300_110));
        assert_eq!(facts.ttl_s, Some(3_600));
    }

    #[test]
    fn missing_or_empty_files_say_nothing() {
        assert_eq!(read_facts(Path::new("/nonexistent/idlepack.jsonl")), CacheFacts::default());
        let t = transcript(&[], 0);
        assert_eq!(read_facts(&t.0), CacheFacts::default());
    }

    /// A temporary file that removes itself, without a crate for it.
    mod tempfile_lite {
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicU32, Ordering};
        pub struct TempPath(pub PathBuf);
        impl Drop for TempPath {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        static N: AtomicU32 = AtomicU32::new(0);
        pub fn path(name: &str) -> TempPath {
            let n = N.fetch_add(1, Ordering::SeqCst);
            TempPath(std::env::temp_dir().join(format!("idlepack-test-{}-{n}-{name}", std::process::id())))
        }
    }
}
