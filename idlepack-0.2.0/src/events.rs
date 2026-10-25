//! `idlepack gain`: what the idle compactions came to, read from the event
//! files both halves write (`~/.idlepack/events/*.json`).

use crate::{fsutil, paths, timeutil};
use serde_json::Value;
use std::fs;

pub struct Event {
    pub app: String,
    pub id: String,
    pub at_ms: i64,
    pub outcome: String,
    pub reason: String,
    pub before: Option<i64>,
    pub after: Option<i64>,
    pub idle_ms: Option<i64>,
}

pub fn load(since_ms: i64) -> Vec<Event> {
    let mut events = Vec::new();
    let Ok(entries) = fs::read_dir(paths::events_dir()) else { return events };
    for entry in entries.flatten() {
        let Some(v) = fsutil::read_json_lossy(&entry.path()) else { continue };
        let at_ms = v.get("at_ms").and_then(Value::as_i64).unwrap_or(0);
        if at_ms < since_ms {
            continue;
        }
        let id = v.get("session_id").or_else(|| v.get("thread_id")).and_then(Value::as_str).unwrap_or("?").to_owned();
        events.push(Event {
            app: v.get("app").and_then(Value::as_str).unwrap_or("?").to_owned(),
            id,
            at_ms,
            outcome: v.get("outcome").and_then(Value::as_str).unwrap_or("?").to_owned(),
            reason: v.get("reason").and_then(Value::as_str).unwrap_or("").to_owned(),
            before: v.get("tokens_before").and_then(Value::as_i64),
            after: v.get("tokens_after").and_then(Value::as_i64),
            idle_ms: v.get("idle_ms").and_then(Value::as_i64),
        });
    }
    events.sort_by_key(|e| std::cmp::Reverse(e.at_ms));
    events
}

pub fn report(days: i64) -> String {
    let now = timeutil::now_ms();
    let events = load(now - days * 86_400_000);
    let compacted: Vec<&Event> = events.iter().filter(|e| e.outcome == "compacted").collect();
    let failed = events.iter().filter(|e| e.outcome == "failed").count();
    let mut out = String::new();
    out.push_str(&format!("idlepack gain · last {days} day(s)\n\n"));
    if compacted.is_empty() {
        out.push_str("No idle compactions yet.\n");
        if failed > 0 {
            out.push_str(&format!("{failed} attempt(s) failed; the reasons are below and in ~/.idlepack/log.\n"));
        } else {
            out.push_str("A session is compacted once it has sat idle for 90% of its cache lifetime with a big enough context.\n");
            out.push_str("`idlepack status` shows what each live session is waiting for.\n");
            return out;
        }
    }
    let by = |app: &str| compacted.iter().filter(|e| e.app == app).count();
    let before: i64 = compacted.iter().filter_map(|e| e.before).sum();
    let measured: Vec<&&Event> = compacted.iter().filter(|e| e.before.is_some() && e.after.is_some()).collect();
    let shrunk: i64 = measured.iter().map(|e| e.before.unwrap_or(0) - e.after.unwrap_or(0)).sum();
    if !compacted.is_empty() {
        out.push_str(&format!(
            "Idle compactions   {}  (Claude {}, Codex {}){}\n",
            compacted.len(),
            by("claude"),
            by("codex"),
            if failed > 0 { format!(" · {failed} failed") } else { String::new() }
        ));
        out.push_str(&format!("Context compacted  {} tokens", timeutil::human_tokens(before)));
        if !measured.is_empty() {
            let after: i64 = measured.iter().filter_map(|e| e.after).sum();
            let pct = if before > 0 {
                100.0 * shrunk as f64 / measured.iter().filter_map(|e| e.before).sum::<i64>() as f64
            } else {
                0.0
            };
            out.push_str(&format!(" -> {} after ({pct:.0}% smaller where measured)", timeutil::human_tokens(after)));
        }
        out.push('\n');
        out.push_str(&format!(
            "Not re-cached      up to {} tokens: what a return after the cache expired would have re-read\n",
            timeutil::human_tokens(if measured.is_empty() { before } else { shrunk })
        ));
        out.push_str(
            "                   (on Claude's 1h cache a re-read is billed as a cache write, 2x the input price;\n",
        );
        out.push_str("                    the compaction itself read the context from the warm cache at 0.1x)\n");
    }
    out.push_str("\nRecent\n");
    for e in events.iter().take(12) {
        let sizes = match (e.before, e.after) {
            (Some(b), Some(a)) => format!("{} -> {}", timeutil::human_tokens(b), timeutil::human_tokens(a)),
            (Some(b), None) => format!("{} -> ?", timeutil::human_tokens(b)),
            _ => "?".into(),
        };
        let idle = e.idle_ms.map(|i| format!("after {} idle", timeutil::human_duration(i))).unwrap_or_default();
        let reason = if e.reason.is_empty() { String::new() } else { format!(" ({})", e.reason) };
        out.push_str(&format!(
            "  {:>8} ago  {:<6} {}  {:<10} {:<16} {idle}{reason}\n",
            timeutil::human_duration(now - e.at_ms),
            e.app,
            &e.id[..e.id.len().min(8)],
            e.outcome,
            sizes
        ));
    }
    out
}
