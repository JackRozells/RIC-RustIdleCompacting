//! The decisions on the Codex side, kept apart from the pipes so they can be
//! tested with a clock in hand: per thread, when its turn ended, how big its
//! context is, and when (if ever) to compact it.
//!
//! A thread is armed when a turn of its completes, disarmed by any activity
//! (a turn starting, the person sending one, a compaction of any kind), and
//! compacted when its timer comes due while it is idle and big enough.

use super::proto::{compact_request, ClientEvent, ServerEvent};
use crate::config::CodexSettings;
use serde_json::{json, Value};
use std::collections::HashMap;

/// A timer this late has lapsed: the computer slept through it and the cache is
/// probably gone. Claude Code's own idle compaction uses the same minute.
pub const LATE_MS: i64 = 60_000;
/// A compaction not seen to finish in this long is given up on.
pub const COMPACTION_DEADLINE_MS: i64 = 10 * 60_000;

#[derive(Clone, Debug, Default)]
pub struct ThreadState {
    pub running: bool,
    pub context: Option<i64>,
    pub window: Option<i64>,
    pub turn_end_ms: Option<i64>,
    pub fire_at_ms: Option<i64>,
    pub compacting: Option<Compacting>,
    pub compacted_at_ms: Option<i64>,
    pub last: Option<Decision>,
    pub seen_ms: i64,
}

#[derive(Clone, Debug)]
pub struct Compacting {
    pub request_id: String,
    pub started_ms: i64,
    pub idle_ms: i64,
    pub before: Option<i64>,
    pub after: Option<i64>,
    pub accepted: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Decision {
    pub outcome: String,
    pub reason: String,
    pub at_ms: i64,
}

#[derive(Debug, PartialEq)]
pub enum Action {
    /// A line to write into the app-server's stdin.
    Inject { thread: String, line: Vec<u8> },
    /// Something to keep for `idlepack gain` and the log.
    Record(Value),
}

pub struct Watcher {
    pub threads: HashMap<String, ThreadState>,
    pending: HashMap<String, String>,
    pub settings: CodexSettings,
    tag: String,
    seq: u64,
}

impl Watcher {
    pub fn new(settings: CodexSettings, tag: String) -> Self {
        Watcher { threads: HashMap::new(), pending: HashMap::new(), settings, tag, seq: 0 }
    }

    pub fn tag(&self) -> &str {
        &self.tag
    }

    fn thread(&mut self, id: &str, now: i64) -> &mut ThreadState {
        let t = self.threads.entry(id.to_owned()).or_default();
        t.seen_ms = now;
        t
    }

    fn decide(t: &mut ThreadState, outcome: &str, reason: impl Into<String>, now: i64) {
        t.last = Some(Decision { outcome: outcome.into(), reason: reason.into(), at_ms: now });
    }

    pub fn on_client(&mut self, event: ClientEvent, now: i64) {
        match event {
            ClientEvent::Activity { thread } | ClientEvent::CompactRequested { thread } => {
                let t = self.thread(&thread, now);
                t.fire_at_ms = None;
            }
        }
    }

    pub fn on_server(&mut self, event: ServerEvent, now: i64) -> Vec<Action> {
        let mut actions = Vec::new();
        let idle_for = self.settings.idle_before_compaction_ms();
        let ttl_s = self.settings.cache_ttl_ms / 1_000;
        match event {
            ServerEvent::TurnStarted { thread } => {
                let t = self.thread(&thread, now);
                if t.compacting.is_none() {
                    t.running = true;
                    t.fire_at_ms = None;
                }
            }
            ServerEvent::TurnCompleted { thread, status, compacted } => {
                let t = self.thread(&thread, now);
                t.running = false;
                if let Some(c) = t.compacting.take() {
                    let ok = status == "completed";
                    t.fire_at_ms = None;
                    if ok {
                        t.compacted_at_ms = Some(now);
                    }
                    let outcome = if ok { "compacted" } else { "failed" };
                    let reason = if ok { String::new() } else { format!("the compaction turn ended {status}") };
                    Self::decide(t, outcome, reason.clone(), now);
                    actions.push(Action::Record(json!({
                        "app": "codex", "thread_id": thread, "at_ms": now, "outcome": outcome, "reason": reason,
                        "idle_ms": c.idle_ms, "tokens_before": c.before, "tokens_after": c.after,
                        "duration_ms": now - c.started_ms, "ttl_s": ttl_s,
                    })));
                } else if compacted {
                    t.compacted_at_ms = Some(now);
                    t.fire_at_ms = None;
                } else {
                    t.turn_end_ms = Some(now);
                    t.fire_at_ms = Some(now + idle_for);
                }
            }
            ServerEvent::TokenUsage { thread, context, window, estimated_only } => {
                let t = self.thread(&thread, now);
                if let Some(c) = t.compacting.as_mut() {
                    if estimated_only {
                        c.after = context;
                    }
                }
                if context.is_some() {
                    t.context = context;
                }
                if window.is_some() {
                    t.window = window;
                }
            }
            ServerEvent::Compacted { thread } => {
                let t = self.thread(&thread, now);
                if t.compacting.is_none() {
                    t.compacted_at_ms = Some(now);
                    t.fire_at_ms = None;
                }
            }
            ServerEvent::OurReply { id, error } => {
                if let Some(thread) = self.pending.remove(&id) {
                    let t = self.thread(&thread, now);
                    match error {
                        Some(message) => {
                            let c = t.compacting.take();
                            Self::decide(t, "failed", message.clone(), now);
                            actions.push(Action::Record(json!({
                                "app": "codex", "thread_id": thread, "at_ms": now, "outcome": "failed",
                                "reason": message, "idle_ms": c.as_ref().map(|c| c.idle_ms),
                                "tokens_before": c.as_ref().and_then(|c| c.before), "ttl_s": ttl_s,
                            })));
                        }
                        None => {
                            if let Some(c) = t.compacting.as_mut() {
                                c.accepted = true;
                            }
                        }
                    }
                }
            }
        }
        actions
    }

    /// Called on a timer: compacts every thread whose idle time has come.
    pub fn tick(&mut self, now: i64) -> Vec<Action> {
        let mut actions = Vec::new();
        let settings = self.settings.clone();
        let mut due: Vec<String> = self
            .threads
            .iter()
            .filter(|(_, t)| t.fire_at_ms.map(|f| f <= now).unwrap_or(false))
            .map(|(id, _)| id.clone())
            .collect();
        due.sort();
        for id in due {
            let tag = self.tag.clone();
            let seq = self.seq;
            let t = self.threads.get_mut(&id).expect("listed");
            let fire_at = t.fire_at_ms.take().expect("due");
            let turn_end = t.turn_end_ms.unwrap_or(fire_at);
            if !settings.enabled {
                Self::decide(t, "skipped", "idle compaction is switched off for Codex", now);
                continue;
            }
            if t.running || t.compacting.is_some() {
                continue;
            }
            if now - fire_at > LATE_MS {
                Self::decide(t, "skipped", "lapsed: the timer fired late (the computer may have slept)", now);
                continue;
            }
            if t.compacted_at_ms.map(|c| c > turn_end).unwrap_or(false) {
                Self::decide(t, "skipped", "already compacted since the turn ended", now);
                continue;
            }
            let min = settings.threshold(t.window);
            match t.context {
                None => {
                    Self::decide(t, "skipped", "the context size is unknown", now);
                    continue;
                }
                Some(n) if n < min => {
                    Self::decide(
                        t,
                        "skipped",
                        format!(
                            "small: {} is under the {} threshold",
                            crate::timeutil::human_tokens(n),
                            crate::timeutil::human_tokens(min)
                        ),
                        now,
                    );
                    continue;
                }
                Some(_) => {}
            }
            let request_id = format!("{tag}{seq}");
            self.seq += 1;
            t.compacting = Some(Compacting {
                request_id: request_id.clone(),
                started_ms: now,
                idle_ms: now - turn_end,
                before: t.context,
                after: None,
                accepted: false,
            });
            self.pending.insert(request_id.clone(), id.clone());
            actions.push(Action::Inject { thread: id.clone(), line: compact_request(&request_id, &id) });
        }
        // Compactions that never reported back.
        let ttl_s = settings.cache_ttl_ms / 1_000;
        for (id, t) in self.threads.iter_mut() {
            if let Some(c) = &t.compacting {
                if now - c.started_ms > COMPACTION_DEADLINE_MS {
                    let c = t.compacting.take().expect("checked");
                    self.pending.remove(&c.request_id);
                    let reason =
                        if c.accepted { "no completion seen within 10 minutes" } else { "no answer within 10 minutes" };
                    Self::decide(t, "failed", reason, now);
                    actions.push(Action::Record(json!({
                        "app": "codex", "thread_id": id, "at_ms": now, "outcome": "failed", "reason": reason,
                        "idle_ms": c.idle_ms, "tokens_before": c.before, "ttl_s": ttl_s,
                    })));
                }
            }
        }
        actions
    }

    /// What `idlepack status` shows for this app-server.
    pub fn snapshot(&self) -> Value {
        let mut threads: Vec<(&String, &ThreadState)> = self.threads.iter().collect();
        threads.sort_by_key(|(_, t)| std::cmp::Reverse(t.seen_ms));
        Value::Array(
            threads
                .into_iter()
                .take(50)
                .map(|(id, t)| {
                    json!({
                        "thread_id": id,
                        "running": t.running,
                        "context_tokens": t.context,
                        "window": t.window,
                        "turn_end_ms": t.turn_end_ms,
                        "fire_at_ms": t.fire_at_ms,
                        "compacting": t.compacting.is_some(),
                        "last": t.last.as_ref().map(|d| json!({ "outcome": d.outcome, "reason": d.reason, "at_ms": d.at_ms })),
                        "seen_ms": t.seen_ms,
                    })
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000;
    const T0: i64 = 1_790_000_000_000;

    fn watcher() -> Watcher {
        Watcher::new(CodexSettings::default(), "idlepack-1-".into())
    }

    fn usage(w: &mut Watcher, thread: &str, context: i64, at: i64) {
        w.on_server(
            ServerEvent::TokenUsage {
                thread: thread.into(),
                context: Some(context),
                window: Some(400_000),
                estimated_only: false,
            },
            at,
        );
    }

    fn turn(w: &mut Watcher, thread: &str, at: i64) {
        w.on_server(ServerEvent::TurnStarted { thread: thread.into() }, at);
        w.on_server(
            ServerEvent::TurnCompleted { thread: thread.into(), status: "completed".into(), compacted: false },
            at,
        );
    }

    fn injected(actions: &[Action]) -> Vec<String> {
        actions
            .iter()
            .filter_map(|a| if let Action::Inject { thread, .. } = a { Some(thread.clone()) } else { None })
            .collect()
    }

    #[test]
    fn compacts_an_idle_thread_at_27_minutes_and_records_it() {
        let mut w = watcher();
        usage(&mut w, "t1", 312_000, T0);
        turn(&mut w, "t1", T0);
        assert!(injected(&w.tick(T0 + 26 * MIN)).is_empty());
        let actions = w.tick(T0 + 27 * MIN);
        assert_eq!(injected(&actions), vec!["t1".to_string()]);
        let Action::Inject { line, .. } = &actions[0] else { panic!() };
        let request: Value = serde_json::from_slice(line).unwrap();
        let id = request["id"].as_str().unwrap().to_owned();
        assert!(id.starts_with("idlepack-1-"));
        // It is not injected twice.
        assert!(injected(&w.tick(T0 + 28 * MIN)).is_empty());
        // The app-server accepts, runs the compaction as a turn, re-estimates the context.
        w.on_server(ServerEvent::OurReply { id, error: None }, T0 + 27 * MIN);
        w.on_server(ServerEvent::TurnStarted { thread: "t1".into() }, T0 + 27 * MIN);
        w.on_server(
            ServerEvent::TokenUsage { thread: "t1".into(), context: Some(17_500), window: None, estimated_only: true },
            T0 + 28 * MIN,
        );
        let done = w.on_server(
            ServerEvent::TurnCompleted { thread: "t1".into(), status: "completed".into(), compacted: true },
            T0 + 28 * MIN,
        );
        let Action::Record(event) = &done[0] else { panic!() };
        assert_eq!(event["outcome"], "compacted");
        assert_eq!(event["tokens_before"], 312_000);
        assert_eq!(event["tokens_after"], 17_500);
        assert_eq!(event["idle_ms"], 27 * MIN);
        // The compaction's own turn does not arm the thread again.
        assert!(injected(&w.tick(T0 + 3 * 60 * MIN)).is_empty());
    }

    #[test]
    fn activity_disarms() {
        let mut w = watcher();
        usage(&mut w, "t1", 312_000, T0);
        turn(&mut w, "t1", T0);
        w.on_client(ClientEvent::Activity { thread: "t1".into() }, T0 + 10 * MIN);
        assert!(injected(&w.tick(T0 + 30 * MIN)).is_empty());
    }

    #[test]
    fn a_new_turn_rearms_from_its_own_end() {
        let mut w = watcher();
        usage(&mut w, "t1", 312_000, T0);
        turn(&mut w, "t1", T0);
        turn(&mut w, "t1", T0 + 20 * MIN);
        assert!(injected(&w.tick(T0 + 27 * MIN)).is_empty());
        assert_eq!(injected(&w.tick(T0 + 47 * MIN)), vec!["t1".to_string()]);
    }

    #[test]
    fn small_threads_and_unknown_sizes_are_left_alone() {
        let mut w = watcher();
        usage(&mut w, "small", 90_000, T0);
        turn(&mut w, "small", T0);
        turn(&mut w, "unknown", T0);
        assert!(injected(&w.tick(T0 + 27 * MIN)).is_empty());
        assert!(w.threads["small"].last.as_ref().unwrap().reason.starts_with("small"));
        assert!(w.threads["unknown"].last.as_ref().unwrap().reason.contains("unknown"));
    }

    #[test]
    fn a_late_timer_has_lapsed() {
        let mut w = watcher();
        usage(&mut w, "t1", 312_000, T0);
        turn(&mut w, "t1", T0);
        assert!(injected(&w.tick(T0 + 90 * MIN)).is_empty());
        assert!(w.threads["t1"].last.as_ref().unwrap().reason.starts_with("lapsed"));
    }

    #[test]
    fn the_persons_own_compaction_stands_the_timer_down() {
        let mut w = watcher();
        usage(&mut w, "t1", 312_000, T0);
        turn(&mut w, "t1", T0);
        w.on_server(ServerEvent::Compacted { thread: "t1".into() }, T0 + 5 * MIN);
        assert!(injected(&w.tick(T0 + 27 * MIN)).is_empty());
        // And a user compaction turn does not arm.
        w.on_server(
            ServerEvent::TurnCompleted { thread: "t1".into(), status: "completed".into(), compacted: true },
            T0 + 6 * MIN,
        );
        assert!(injected(&w.tick(T0 + 40 * MIN)).is_empty());
    }

    #[test]
    fn a_refusal_is_recorded_as_a_failure() {
        let mut w = watcher();
        usage(&mut w, "t1", 312_000, T0);
        turn(&mut w, "t1", T0);
        let actions = w.tick(T0 + 27 * MIN);
        let Action::Inject { line, .. } = &actions[0] else { panic!() };
        let id = serde_json::from_slice::<Value>(line).unwrap()["id"].as_str().unwrap().to_owned();
        let recorded =
            w.on_server(ServerEvent::OurReply { id, error: Some("thread not found: t1".into()) }, T0 + 27 * MIN);
        let Action::Record(event) = &recorded[0] else { panic!() };
        assert_eq!(event["outcome"], "failed");
        assert!(w.threads["t1"].compacting.is_none());
    }

    #[test]
    fn a_compaction_that_never_finishes_is_given_up_on() {
        let mut w = watcher();
        usage(&mut w, "t1", 312_000, T0);
        turn(&mut w, "t1", T0);
        w.tick(T0 + 27 * MIN);
        let actions = w.tick(T0 + 38 * MIN);
        assert!(matches!(&actions[0], Action::Record(e) if e["outcome"] == "failed"));
        assert!(w.threads["t1"].compacting.is_none());
    }

    #[test]
    fn switched_off_does_nothing() {
        let settings = CodexSettings { enabled: false, ..CodexSettings::default() };
        let mut w = Watcher::new(settings, "idlepack-1-".into());
        usage(&mut w, "t1", 312_000, T0);
        turn(&mut w, "t1", T0);
        assert!(injected(&w.tick(T0 + 27 * MIN)).is_empty());
    }

    #[test]
    fn threads_are_independent() {
        let mut w = watcher();
        usage(&mut w, "a", 312_000, T0);
        usage(&mut w, "b", 250_000, T0);
        turn(&mut w, "a", T0);
        turn(&mut w, "b", T0 + 5 * MIN);
        assert_eq!(injected(&w.tick(T0 + 27 * MIN)), vec!["a".to_string()]);
        assert_eq!(injected(&w.tick(T0 + 32 * MIN)), vec!["b".to_string()]);
    }
}
