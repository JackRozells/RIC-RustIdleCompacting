//! `~/.idlepack/config.json`: one file for both halves. The Claude mod reads
//! the `"claude"` section itself (it runs inside Claude Code, in JavaScript);
//! this side reads `"codex"` and shows and edits both.

use crate::{fsutil, paths};
use serde_json::{json, Map, Value};
use std::io;

/// The defaults, as they are written into a fresh config file.
pub fn defaults() -> Value {
    json!({
        "claude": {
            "enabled": true,
            "minTokens": "auto",
            "fireAtFraction": 0.9,
            "terminal": "auto",
            "terminalFireAtFraction": 0.92,
            "allowFiveMinuteCache": false,
            "instructions": "",
            "toast": true
        },
        "codex": {
            "enabled": true,
            "minTokens": "auto",
            "cacheTtlMinutes": 30,
            "fireAtFraction": 0.9,
            "realCodexPath": null
        }
    })
}

/// The smallest threshold used: below it a compaction would save next to nothing.
pub const FLOOR_TOKENS: i64 = 20_000;
/// The most `auto` asks for, whatever the window.
pub const AUTO_CAP_TOKENS: i64 = 200_000;
/// Below this, `config set` says a compaction saves little: Claude Code's system
/// prompt and tools alone are about 45k of every request, and stay after one.
pub const SMALL_TOKENS: i64 = 60_000;

/// How big a session's context has to be before idle compaction touches it:
/// `minTokens` in either section of the config. Written as `"auto"`, a token
/// count (`150000`, or `"150k"`, `"1.5m"`), or a share of the model's context
/// window (`"40%"`). The Claude mod reads the same spellings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Threshold {
    /// Half the model's window, at most 200k.
    Auto,
    Tokens(i64),
    /// Per cent of the model's window.
    Percent(f64),
}

impl Threshold {
    /// Reads what a person might type or write in the file. None for anything else.
    pub fn parse_text(text: &str) -> Option<Threshold> {
        let t = text.trim().to_ascii_lowercase().replace([',', '_', ' '], "");
        if t == "auto" {
            return Some(Threshold::Auto);
        }
        // Digits and at most one point: no signs, exponents or words.
        let plain =
            |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || c == '.') && s.matches('.').count() <= 1;
        if let Some(p) = t.strip_suffix('%') {
            if !plain(p) {
                return None;
            }
            let p: f64 = p.parse().ok()?;
            return (p.is_finite() && p > 0.0 && p <= 100.0).then_some(Threshold::Percent(p.clamp(5.0, 95.0)));
        }
        let (digits, scale) = if let Some(d) = t.strip_suffix('k') {
            (d, 1_000.0)
        } else if let Some(d) = t.strip_suffix('m') {
            (d, 1_000_000.0)
        } else {
            (t.as_str(), 1.0)
        };
        if !plain(digits) {
            return None;
        }
        let n: f64 = digits.parse().ok()?;
        (n.is_finite() && n > 0.0).then(|| Threshold::Tokens((n * scale).round() as i64))
    }

    pub fn parse(value: &Value) -> Option<Threshold> {
        match value {
            Value::Number(n) => n.as_f64().filter(|n| *n > 0.0).map(|n| Threshold::Tokens(n.round() as i64)),
            Value::String(s) => Threshold::parse_text(s),
            _ => None,
        }
    }

    /// The token count it comes to for a model with this window (when known).
    pub fn tokens(&self, window: Option<i64>) -> i64 {
        let window = window.filter(|w| *w > 0);
        let auto = || window.map(|w| w / 2).unwrap_or(AUTO_CAP_TOKENS).clamp(FLOOR_TOKENS, AUTO_CAP_TOKENS);
        match *self {
            Threshold::Auto => auto(),
            Threshold::Tokens(n) => n.max(FLOOR_TOKENS),
            Threshold::Percent(p) => match window {
                Some(w) => ((w as f64 * p / 100.0).round() as i64).max(FLOOR_TOKENS),
                None => auto(),
            },
        }
    }

    /// How it is stored: a plain number for a token count, else its spelling.
    pub fn to_value(self) -> Value {
        match self {
            Threshold::Auto => Value::String("auto".into()),
            Threshold::Tokens(n) => json!(n.max(FLOOR_TOKENS)),
            Threshold::Percent(p) => Value::String(format!("{}%", trim_float(p))),
        }
    }

    pub fn describe(&self) -> String {
        match *self {
            Threshold::Auto => "auto (half the model's window, at most 200k)".into(),
            Threshold::Tokens(n) => crate::timeutil::human_tokens(n.max(FLOOR_TOKENS)),
            Threshold::Percent(p) => format!("{}% of the model's window", trim_float(p)),
        }
    }
}

fn trim_float(p: f64) -> String {
    let text = format!("{p:.1}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// The threshold set for one app (`"claude"` or `"codex"`), Auto when unset or unreadable.
pub fn threshold_for(config: &Value, app: &str) -> Threshold {
    config.get(app).and_then(|s| s.get("minTokens")).and_then(Threshold::parse).unwrap_or(Threshold::Auto)
}

/// The Codex side's settings, read and checked.
#[derive(Clone, Debug)]
pub struct CodexSettings {
    pub enabled: bool,
    pub min_tokens: Threshold,
    pub cache_ttl_ms: i64,
    pub fire_at_fraction: f64,
    pub real_codex_path: Option<String>,
    /// How often the shim checks its timers. Not in the defaults: tests turn it down.
    pub tick_ms: u64,
    /// Tests only (`codex._testIdleMs`): the idle time in milliseconds, past the one-minute floor.
    pub test_idle_ms: Option<i64>,
}

impl Default for CodexSettings {
    fn default() -> Self {
        CodexSettings::from_value(&defaults())
    }
}

impl CodexSettings {
    pub fn from_value(config: &Value) -> Self {
        let given = config.get("codex").cloned().unwrap_or(Value::Null);
        let enabled = given.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        let min_tokens = threshold_for(config, "codex");
        let ttl_minutes = given
            .get("cacheTtlMinutes")
            .and_then(Value::as_f64)
            .filter(|m| m.is_finite() && *m >= 1.0)
            .unwrap_or(30.0)
            .min(24.0 * 60.0);
        let fraction = given
            .get("fireAtFraction")
            .and_then(Value::as_f64)
            .filter(|f| f.is_finite())
            .unwrap_or(0.9)
            .clamp(0.5, 0.95);
        let real_codex_path =
            given.get("realCodexPath").and_then(Value::as_str).map(str::to_owned).filter(|s| !s.trim().is_empty());
        let tick_ms = given.get("tickMs").and_then(Value::as_u64).unwrap_or(5_000).clamp(10, 60_000);
        let test_idle_ms = given.get("_testIdleMs").and_then(Value::as_i64).filter(|ms| *ms > 0);
        CodexSettings {
            enabled,
            min_tokens,
            cache_ttl_ms: (ttl_minutes * 60_000.0) as i64,
            fire_at_fraction: fraction,
            real_codex_path,
            tick_ms,
            test_idle_ms,
        }
    }

    pub fn load() -> Self {
        CodexSettings::from_value(&load())
    }

    pub fn threshold(&self, window: Option<i64>) -> i64 {
        self.min_tokens.tokens(window)
    }

    pub fn idle_before_compaction_ms(&self) -> i64 {
        self.test_idle_ms.unwrap_or((self.cache_ttl_ms as f64 * self.fire_at_fraction) as i64)
    }
}

/// The config as stored, over the defaults (a missing or unreadable file reads as the defaults).
pub fn load() -> Value {
    let mut merged = defaults();
    if let Some(stored) = fsutil::read_json_lossy(&paths::config_file()) {
        merge(&mut merged, &stored);
    }
    merged
}

fn merge(base: &mut Value, over: &Value) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (key, value) in o {
                match b.get_mut(key) {
                    Some(existing) if existing.is_object() && value.is_object() => merge(existing, value),
                    _ => {
                        b.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (b, o) => *b = o.clone(),
    }
}

/// Writes the defaults when there is no config yet; leaves an existing one alone.
pub fn ensure_file() -> io::Result<bool> {
    let path = paths::config_file();
    if path.exists() {
        return Ok(false);
    }
    fsutil::write_json(&path, &defaults())?;
    Ok(true)
}

/// What `set` stored, for the caller to report.
pub struct Stored {
    pub keys: Vec<String>,
    pub value: Value,
    pub notes: Vec<String>,
}

/// `config set claude.minTokens 150k`, or the shorthand `config set minTokens 150k`
/// for both apps at once. A threshold is checked and stored in its plain form;
/// any other value is taken as JSON when it parses, else as a string.
pub fn set(dotted: &str, raw: &str) -> io::Result<Stored> {
    let bare = dotted.trim();
    if bare == "minTokens" || bare.ends_with(".minTokens") {
        let threshold = Threshold::parse_text(raw).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("`{raw}` is not a threshold: use a token count (150000, 150k, 1.5m), a share of the window (40%), or auto"),
            )
        })?;
        let keys: Vec<String> = if bare == "minTokens" {
            vec!["claude.minTokens".into(), "codex.minTokens".into()]
        } else {
            vec![bare.to_owned()]
        };
        let mut notes = Vec::new();
        if let Threshold::Tokens(n) = threshold {
            if n < FLOOR_TOKENS {
                notes.push(format!("raised to the {} minimum", crate::timeutil::human_tokens(FLOOR_TOKENS)));
            } else if n < SMALL_TOKENS {
                notes.push(
                    "below ~60k a compaction saves little: the system prompt and tools stay in every request \
                     (about 45k in Claude Code)"
                        .into(),
                );
            }
        }
        let value = threshold.to_value();
        for key in &keys {
            set_raw(key, value.clone())?;
        }
        return Ok(Stored { keys, value, notes });
    }
    let value: Value = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned()));
    set_raw(bare, value.clone())?;
    Ok(Stored { keys: vec![bare.to_owned()], value, notes: vec![] })
}

fn set_raw(dotted: &str, value: Value) -> io::Result<()> {
    let parts: Vec<&str> = dotted.split('.').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "an empty key"));
    }
    let known = defaults();
    let mut probe = &known;
    for part in &parts {
        probe = match probe.get(part) {
            Some(v) => v,
            None if parts[0] == "codex" && *part == "tickMs" => &Value::Null,
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown setting `{dotted}` (see `idlepack config`)"),
                ))
            }
        };
    }
    let path = paths::config_file();
    let mut stored = fsutil::read_json(&path)?.unwrap_or_else(defaults);
    if !stored.is_object() {
        stored = Value::Object(Map::new());
    }
    let mut cursor = &mut stored;
    for part in &parts[..parts.len() - 1] {
        if !cursor.get(*part).map(Value::is_object).unwrap_or(false) {
            cursor.as_object_mut().expect("object").insert((*part).to_owned(), Value::Object(Map::new()));
        }
        cursor = cursor.get_mut(*part).expect("just inserted");
    }
    cursor.as_object_mut().expect("object").insert(parts[parts.len() - 1].to_owned(), value);
    fsutil::write_json(&path, &stored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_defaults_fire_at_27_minutes() {
        let s = CodexSettings::default();
        assert!(s.enabled);
        assert_eq!(s.cache_ttl_ms, 30 * 60_000);
        assert_eq!(s.idle_before_compaction_ms(), 27 * 60_000);
        assert_eq!(s.threshold(Some(1_000_000)), 200_000);
        assert_eq!(s.threshold(Some(272_000)), 136_000);
        assert_eq!(s.threshold(Some(64_000)), 32_000);
        assert_eq!(s.threshold(Some(30_000)), FLOOR_TOKENS);
        assert_eq!(s.threshold(None), 200_000);
    }

    #[test]
    fn codex_settings_are_checked() {
        let s = CodexSettings::from_value(&json!({
            "codex": { "minTokens": 10, "fireAtFraction": 3.0, "cacheTtlMinutes": 60, "realCodexPath": "  " }
        }));
        assert_eq!(s.threshold(Some(1_000_000)), FLOOR_TOKENS);
        assert_eq!(s.fire_at_fraction, 0.95);
        assert_eq!(s.cache_ttl_ms, 3_600_000);
        assert_eq!(s.real_codex_path, None);
    }

    #[test]
    fn thresholds_read_the_ways_people_write_them() {
        let p = Threshold::parse_text;
        assert_eq!(p("auto"), Some(Threshold::Auto));
        assert_eq!(p(" AUTO "), Some(Threshold::Auto));
        assert_eq!(p("150000"), Some(Threshold::Tokens(150_000)));
        assert_eq!(p("150,000"), Some(Threshold::Tokens(150_000)));
        assert_eq!(p("150k"), Some(Threshold::Tokens(150_000)));
        assert_eq!(p("150K"), Some(Threshold::Tokens(150_000)));
        assert_eq!(p("1.5m"), Some(Threshold::Tokens(1_500_000)));
        assert_eq!(p("40%"), Some(Threshold::Percent(40.0)));
        assert_eq!(p("99%"), Some(Threshold::Percent(95.0)));
        assert_eq!(p("1%"), Some(Threshold::Percent(5.0)));
        for bad in ["", "lots", "-5k", "0", "150kb", "0%", "120%", "1e999", "1e3k", "1.2.3k", "+5k"] {
            assert_eq!(p(bad), None, "{bad}");
        }
        assert_eq!(Threshold::parse(&json!(300000)), Some(Threshold::Tokens(300_000)));
        assert_eq!(Threshold::parse(&json!("300k")), Some(Threshold::Tokens(300_000)));
        assert_eq!(Threshold::parse(&json!(true)), None);
    }

    #[test]
    fn thresholds_come_to_token_counts() {
        assert_eq!(Threshold::Tokens(150_000).tokens(Some(1_000_000)), 150_000);
        assert_eq!(Threshold::Tokens(150_000).tokens(None), 150_000);
        assert_eq!(Threshold::Tokens(5_000).tokens(None), FLOOR_TOKENS);
        assert_eq!(Threshold::Percent(40.0).tokens(Some(1_000_000)), 400_000);
        assert_eq!(Threshold::Percent(40.0).tokens(Some(200_000)), 80_000);
        assert_eq!(Threshold::Percent(5.0).tokens(Some(200_000)), FLOOR_TOKENS);
        // Window unknown: a share of it falls back to auto's rule.
        assert_eq!(Threshold::Percent(40.0).tokens(None), AUTO_CAP_TOKENS);
        assert_eq!(Threshold::Tokens(150_000).to_value(), json!(150000));
        assert_eq!(Threshold::Percent(37.5).to_value(), json!("37.5%"));
        assert_eq!(Threshold::Auto.to_value(), json!("auto"));
    }

    #[test]
    fn merging_keeps_unknown_keys_and_defaults() {
        let mut base = defaults();
        merge(&mut base, &json!({ "claude": { "minTokens": 150000 }, "extra": 1 }));
        assert_eq!(base["claude"]["minTokens"], json!(150000));
        assert_eq!(base["claude"]["fireAtFraction"], json!(0.9));
        assert_eq!(base["extra"], json!(1));
    }
}
