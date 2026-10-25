//! The Codex half: a shim the desktop app runs as its app-server (see
//! `shim.rs`), installed by pointing `CODEX_CLI_PATH` at a copy of this binary.

pub mod locate;
pub mod proto;
pub mod shim;
pub mod watcher;

use crate::config::CodexSettings;
use crate::{fsutil, paths, timeutil, userenv};
use serde_json::{json, Value};
use std::fs;
use std::io;
use std::path::Path;

pub const CLI_ENV: &str = "CODEX_CLI_PATH";

fn slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Copies this binary to the shim's place. A running shim on Windows cannot be
/// overwritten, but it can be renamed out of the way first.
fn place_shim(exe: &Path) -> io::Result<()> {
    let target = paths::shim_exe();
    fs::create_dir_all(paths::shim_dir())?;
    if target.exists() {
        if let Err(err) = fs::remove_file(&target) {
            let aside = target.with_extension(format!("old-{}", timeutil::now_ms()));
            fs::rename(&target, &aside).map_err(|_| err)?;
        }
    }
    fs::copy(exe, &target)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755))?;
    }
    // Old copies left aside by an earlier install, once nothing runs them.
    if let Ok(entries) = fs::read_dir(paths::shim_dir()) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().contains(".old-") {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    Ok(())
}

pub fn install(exe: &Path) -> io::Result<Vec<String>> {
    let mut lines = Vec::new();
    let settings = CodexSettings::load();
    let shim = paths::shim_exe();
    let current = userenv::get(CLI_ENV).unwrap_or(None);
    let mut record = fsutil::read_json_lossy(&paths::install_record()).unwrap_or_else(|| json!({}));
    let ours = |p: &str| locate::is_ours(Path::new(p)) || Path::new(p) == shim;
    let previous = match &current {
        Some(p) if !ours(p) => Some(p.clone()),
        _ => record.pointer("/codex/previousCliPath").and_then(Value::as_str).map(str::to_owned),
    };

    place_shim(exe)?;
    lines.push(format!("placed the Codex shim at {}", paths::pretty(&shim)));
    match locate::real_codex(&settings) {
        Some((real, how)) => lines.push(format!("it will run the real codex at {} ({how})", real.display())),
        None => lines.push(format!(
            "warning: no real codex found yet; install the Codex desktop app, or set codex.realCodexPath (or {})",
            locate::REAL_ENV
        )),
    }
    if let Some(prev) = &previous {
        lines.push(format!("{CLI_ENV} was {prev}: the shim runs that, and uninstall puts it back"));
    }
    userenv::set(CLI_ENV, &shim.to_string_lossy())?;
    lines.push(format!("set {CLI_ENV} -> the shim ({})", userenv::how_it_applies()));

    if let Some(map) = record.as_object_mut() {
        map.insert(
            "codex".into(),
            json!({ "shim": slash(&shim), "previousCliPath": previous, "installedAtMs": timeutil::now_ms() }),
        );
    }
    fsutil::write_json(&paths::install_record(), &record)?;
    Ok(lines)
}

pub fn uninstall() -> io::Result<Vec<String>> {
    let mut lines = Vec::new();
    let mut record = fsutil::read_json_lossy(&paths::install_record()).unwrap_or_else(|| json!({}));
    let previous = record.pointer("/codex/previousCliPath").and_then(Value::as_str).map(str::to_owned);
    let current = userenv::get(CLI_ENV).unwrap_or(None);
    let points_at_shim =
        current.as_deref().map(|c| locate::is_ours(Path::new(c)) || Path::new(c) == paths::shim_exe()).unwrap_or(false);
    if points_at_shim {
        match &previous {
            Some(prev) => {
                userenv::set(CLI_ENV, prev)?;
                lines.push(format!("put {CLI_ENV} back to {prev}"));
            }
            None => {
                userenv::clear(CLI_ENV)?;
                lines.push(format!("removed {CLI_ENV} ({})", userenv::how_it_applies()));
            }
        }
    } else if current.is_some() {
        lines.push(format!("left {CLI_ENV} alone: it no longer points at the shim"));
    }
    if paths::shim_dir().exists() {
        // A running shim (Windows) keeps its file; it goes with the next uninstall or reboot.
        match fs::remove_dir_all(paths::shim_dir()) {
            Ok(()) => lines.push(format!("deleted {}", paths::pretty(&paths::shim_dir()))),
            Err(err) => lines.push(format!(
                "could not delete {} yet ({err}); quit the Codex app and run uninstall again",
                paths::pretty(&paths::shim_dir())
            )),
        }
    }
    if let Some(map) = record.as_object_mut() {
        map.remove("codex");
    }
    fsutil::write_json(&paths::install_record(), &record)?;
    Ok(lines)
}

/// Lines for `idlepack status`.
pub fn status_lines(now: i64) -> Vec<String> {
    let mut lines = Vec::new();
    let settings = CodexSettings::load();
    let shim = paths::shim_exe();
    let current = userenv::get(CLI_ENV).unwrap_or(None);
    let points = current.as_deref().map(|c| Path::new(c) == shim || locate::is_ours(Path::new(c))).unwrap_or(false);
    lines.push(format!(
        "shim            {}",
        if shim.exists() {
            format!("{} ✓", paths::pretty(&shim))
        } else {
            "not installed (run `idlepack init --codex`)".into()
        }
    ));
    lines.push(format!(
        "{CLI_ENV}  {}",
        match (&current, points) {
            (Some(_), true) => format!("-> the shim ✓ ({})", userenv::store_name()),
            (Some(other), false) => format!("-> {other} (not the shim)"),
            (None, _) => "not set".into(),
        }
    ));
    match locate::real_codex(&settings) {
        Some((real, how)) => lines.push(format!("real codex      {} ({how})", real.display())),
        None => lines.push("real codex      not found".into()),
    }
    lines.push(format!(
        "timing          compacts after {} idle (90% of a {} cache lifetime)",
        timeutil::human_duration(settings.idle_before_compaction_ms()),
        timeutil::human_duration(settings.cache_ttl_ms),
    ));
    lines.push(format!("threshold       {}  (codex.minTokens)", settings.min_tokens.describe()));
    let mut live = 0;
    if let Ok(entries) = fs::read_dir(paths::codex_live_dir()) {
        for entry in entries.flatten() {
            let Some(status) = fsutil::read_json_lossy(&entry.path()) else { continue };
            let beat = status.get("beat_ms").and_then(Value::as_i64).unwrap_or(0);
            if now - beat > 2 * 60_000 {
                // A shim that stopped without cleaning up (killed with its app).
                if now - beat > 24 * 3_600_000 {
                    let _ = fs::remove_file(entry.path());
                }
                continue;
            }
            live += 1;
            let pid = status.get("pid").and_then(Value::as_i64).unwrap_or(0);
            let threads = status.get("threads").and_then(Value::as_array).cloned().unwrap_or_default();
            lines.push(format!("app-server      pid {pid}, {} thread(s) seen", threads.len()));
            for t in threads.iter().take(8) {
                let id = t.get("thread_id").and_then(Value::as_str).unwrap_or("?");
                let ctx = t
                    .get("context_tokens")
                    .and_then(Value::as_i64)
                    .map(timeutil::human_tokens)
                    .unwrap_or_else(|| "?".into());
                let context = t.get("context_tokens").and_then(Value::as_i64);
                let threshold = settings.threshold(t.get("window").and_then(Value::as_i64));
                let small = context.map(|c| c < threshold).unwrap_or(false);
                let mut state = if t.get("running").and_then(Value::as_bool).unwrap_or(false) {
                    "running".to_owned()
                } else if t.get("compacting").and_then(Value::as_bool).unwrap_or(false) {
                    "compacting".to_owned()
                } else if small {
                    format!("idle, under the {} threshold: left alone", timeutil::human_tokens(threshold))
                } else if let Some(fire) = t.get("fire_at_ms").and_then(Value::as_i64) {
                    format!(
                        "idle, compacts in {} (over the {} threshold)",
                        timeutil::human_duration(fire - now),
                        timeutil::human_tokens(threshold)
                    )
                } else {
                    "idle".to_owned()
                };
                if let Some(outcome) = t.pointer("/last/outcome").and_then(Value::as_str) {
                    let reason = t.pointer("/last/reason").and_then(Value::as_str).unwrap_or("");
                    state.push_str(&format!(
                        " · last: {outcome}{}",
                        if reason.is_empty() { String::new() } else { format!(" ({reason})") }
                    ));
                }
                lines.push(format!("  {}  {:>6} ctx  {state}", &id[..id.len().min(8)], ctx));
            }
        }
    }
    if live == 0 {
        lines.push("app-server      none running through the shim (restart the Codex app after init)".into());
    }
    lines
}
