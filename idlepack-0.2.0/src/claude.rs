//! The Claude Code half: a mod that compacts from inside the session (so it
//! works in the Desktop app's Code tab, where Claude Code's own idle compaction
//! does not run), and a Stop hook that tells it the session's cache lifetime.
//!
//! `init` writes the mod to `~/.idlepack/claude-mod`, names that folder in
//! `CLAUDE_CODE_PLUGIN_DIRS` in `~/.claude/settings.json` (the one way to load a
//! mod into the Desktop app, which takes no command-line flags), and adds the
//! Stop hook there. `uninstall` takes out exactly what `init` put in.

use crate::{fsutil, paths, timeutil, transcript};
use serde_json::{json, Map, Value};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// The mod's files, built into the binary so the install is one file.
const MOD_FILES: &[(&str, &str)] = &[
    (".claude-plugin/plugin.json", include_str!("../mod/.claude-plugin/plugin.json")),
    ("hooks/hooks.json", include_str!("../mod/hooks/hooks.json")),
    ("hooks/register.js", include_str!("../mod/hooks/register.js")),
    ("hooks/policy.js", include_str!("../mod/hooks/policy.js")),
    ("README.md", include_str!("../mod/README.md")),
];

const PLUGIN_DIRS: &str = "CLAUDE_CODE_PLUGIN_DIRS";
/// Mods were early access before Claude Code 2.1.287 and needed this; later builds ignore it.
const FUNCTION_HOOKS: &str = "CLAUDE_CODE_ENABLE_FUNCTION_HOOKS";
const HOOK_MARKER: &str = " hook claude-stop";

fn list_separator() -> char {
    if cfg!(windows) {
        ';'
    } else {
        ':'
    }
}

pub fn settings_path() -> PathBuf {
    paths::claude_dir().join("settings.json")
}

fn slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// The Stop hook's command line. Claude Code runs hooks through Git Bash on
/// Windows when it has it and through PowerShell when not, so the command is
/// spelled to work in both: an unquoted path when it has no spaces, and an
/// explicit PowerShell call (with `"shell": "powershell"`) when it has.
pub fn hook_command(exe: &Path) -> (String, Option<&'static str>) {
    let p = slash(exe);
    let plain = p.chars().all(|c| c.is_ascii_alphanumeric() || "/._-:~+@".contains(c));
    if plain {
        (format!("{p}{HOOK_MARKER}"), None)
    } else if cfg!(windows) {
        (format!("& '{}'{HOOK_MARKER}", p.replace('\'', "''")), Some("powershell"))
    } else {
        (format!("'{}'{HOOK_MARKER}", p.replace('\'', "'\\''")), None)
    }
}

fn is_our_hook(hook: &Value) -> bool {
    hook.get("command").and_then(Value::as_str).map(|c| c.contains(HOOK_MARKER)).unwrap_or(false)
}

fn obj(value: &mut Value) -> &mut Map<String, Value> {
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    value.as_object_mut().expect("object")
}

/// What an edit of settings.json did, for the report.
#[derive(Default, Debug)]
pub struct SettingsChange {
    pub added_plugin_dir: bool,
    pub added_function_hooks: bool,
    pub added_stop_hook: bool,
    pub replaced_stop_hook: bool,
}

/// Adds the mod folder and the Stop hook to a settings object. Pure, so it is tested as is.
pub fn add_to_settings(settings: &mut Value, mod_dir: &str, exe: &Path) -> SettingsChange {
    let mut change = SettingsChange::default();
    let root = obj(settings);
    let env = obj(root.entry("env").or_insert_with(|| json!({})));
    let sep = list_separator();
    let current = env.get(PLUGIN_DIRS).and_then(Value::as_str).unwrap_or("").to_owned();
    let mut dirs: Vec<String> =
        current.split(sep).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect();
    let norm = |s: &str| s.replace('\\', "/").trim_end_matches('/').to_lowercase();
    if !dirs.iter().any(|d| norm(d) == norm(mod_dir)) {
        dirs.push(mod_dir.to_owned());
        change.added_plugin_dir = true;
    }
    env.insert(PLUGIN_DIRS.into(), Value::String(dirs.join(&sep.to_string())));
    if !env.contains_key(FUNCTION_HOOKS) {
        env.insert(FUNCTION_HOOKS.into(), Value::String("1".into()));
        change.added_function_hooks = true;
    }

    let (command, shell) = hook_command(exe);
    let mut hook = json!({ "type": "command", "command": command, "timeout": 10 });
    if let Some(shell) = shell {
        hook["shell"] = Value::String(shell.into());
    }
    let hooks = obj(root.entry("hooks").or_insert_with(|| json!({})));
    let stop = hooks.entry("Stop").or_insert_with(|| json!([]));
    if !stop.is_array() {
        *stop = json!([]);
    }
    let groups = stop.as_array_mut().expect("array");
    let mut found = false;
    for group in groups.iter_mut() {
        if let Some(list) = group.get_mut("hooks").and_then(Value::as_array_mut) {
            for existing in list.iter_mut() {
                if is_our_hook(existing) {
                    if !found && *existing != hook {
                        *existing = hook.clone();
                        change.replaced_stop_hook = true;
                    }
                    found = true;
                }
            }
        }
    }
    if !found {
        groups.push(json!({ "hooks": [hook] }));
        change.added_stop_hook = true;
    }
    change
}

/// Takes out what `add_to_settings` put in. `drop_function_hooks` only when init added it.
pub fn remove_from_settings(settings: &mut Value, mod_dir: &str, drop_function_hooks: bool) -> bool {
    let mut changed = false;
    let Some(root) = settings.as_object_mut() else { return false };
    if let Some(env) = root.get_mut("env").and_then(Value::as_object_mut) {
        let sep = list_separator();
        let norm = |s: &str| s.replace('\\', "/").trim_end_matches('/').to_lowercase();
        if let Some(current) = env.get(PLUGIN_DIRS).and_then(Value::as_str).map(str::to_owned) {
            let kept: Vec<&str> =
                current.split(sep).map(str::trim).filter(|s| !s.is_empty() && norm(s) != norm(mod_dir)).collect();
            if kept.len() != current.split(sep).filter(|s| !s.trim().is_empty()).count() {
                changed = true;
            }
            if kept.is_empty() {
                env.remove(PLUGIN_DIRS);
            } else {
                env.insert(PLUGIN_DIRS.into(), Value::String(kept.join(&sep.to_string())));
            }
        }
        if drop_function_hooks && env.remove(FUNCTION_HOOKS).is_some() {
            changed = true;
        }
        if env.is_empty() {
            root.remove("env");
        }
    }
    if let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) {
        if let Some(groups) = hooks.get_mut("Stop").and_then(Value::as_array_mut) {
            for group in groups.iter_mut() {
                if let Some(list) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                    let before = list.len();
                    list.retain(|h| !is_our_hook(h));
                    changed |= list.len() != before;
                }
            }
            groups.retain(|g| g.get("hooks").and_then(Value::as_array).map(|l| !l.is_empty()).unwrap_or(true));
            if groups.is_empty() {
                hooks.remove("Stop");
            }
        }
        if hooks.is_empty() {
            root.remove("hooks");
        }
    }
    changed
}

pub fn write_mod(dir: &Path) -> io::Result<()> {
    for (rel, text) in MOD_FILES {
        fsutil::write_atomic(&dir.join(rel), text.as_bytes())?;
    }
    Ok(())
}

pub struct InstallReport {
    pub lines: Vec<String>,
}

pub fn install(exe: &Path) -> io::Result<InstallReport> {
    let mut lines = Vec::new();
    let mod_dir = paths::mod_dir();
    write_mod(&mod_dir)?;
    lines.push(format!("wrote the Claude Code mod to {}", paths::pretty(&mod_dir)));

    let path = settings_path();
    let mut settings = fsutil::read_json(&path)
        .map_err(|e| {
            io::Error::new(e.kind(), format!("{} is not valid JSON, so it was left alone: {e}", path.display()))
        })?
        .unwrap_or_else(|| json!({}));
    let backup = path.with_file_name("settings.json.idlepack-backup");
    if path.exists() && !backup.exists() {
        fs::copy(&path, &backup)?;
        lines.push(format!("backed up {} to {}", paths::pretty(&path), paths::pretty(&backup)));
    }
    let change = add_to_settings(&mut settings, &slash(&mod_dir), exe);
    fsutil::write_json(&path, &settings)?;
    lines.push(format!(
        "{} {PLUGIN_DIRS} in {}",
        if change.added_plugin_dir { "added the mod to" } else { "the mod was already in" },
        paths::pretty(&path)
    ));
    if change.added_stop_hook || change.replaced_stop_hook {
        lines.push(format!(
            "{} the Stop hook (`idlepack hook claude-stop`)",
            if change.added_stop_hook { "added" } else { "updated" }
        ));
    }
    let mut record = fsutil::read_json_lossy(&paths::install_record()).unwrap_or_else(|| json!({}));
    let added_before = record.pointer("/claude/addedFunctionHooksEnv").and_then(Value::as_bool).unwrap_or(false);
    obj(&mut record).insert(
        "claude".into(),
        json!({
            "settings": slash(&path),
            "modDir": slash(&mod_dir),
            "addedFunctionHooksEnv": change.added_function_hooks || added_before,
            "installedAtMs": timeutil::now_ms(),
        }),
    );
    fsutil::write_json(&paths::install_record(), &record)?;
    Ok(InstallReport { lines })
}

pub fn uninstall() -> io::Result<Vec<String>> {
    let mut lines = Vec::new();
    let record = fsutil::read_json_lossy(&paths::install_record()).unwrap_or_else(|| json!({}));
    let mod_dir = paths::mod_dir();
    let path = settings_path();
    if let Some(mut settings) = fsutil::read_json(&path)? {
        let drop_fh = record.pointer("/claude/addedFunctionHooksEnv").and_then(Value::as_bool).unwrap_or(false);
        if remove_from_settings(&mut settings, &slash(&mod_dir), drop_fh) {
            fsutil::write_json(&path, &settings)?;
            lines.push(format!("removed the mod and the Stop hook from {}", paths::pretty(&path)));
        }
    }
    if mod_dir.exists() {
        fs::remove_dir_all(&mod_dir)?;
        lines.push(format!("deleted {}", paths::pretty(&mod_dir)));
    }
    let mut record = record;
    if let Some(map) = record.as_object_mut() {
        map.remove("claude");
    }
    fsutil::write_json(&paths::install_record(), &record)?;
    Ok(lines)
}

/// Whether the settings currently load the mod and run the Stop hook.
pub fn installed_state() -> (bool, bool) {
    let Some(settings) = fsutil::read_json_lossy(&settings_path()) else { return (false, false) };
    let mod_dir = slash(&paths::mod_dir()).to_lowercase();
    let in_dirs = settings
        .pointer(&format!("/env/{PLUGIN_DIRS}"))
        .and_then(Value::as_str)
        .map(|s| s.replace('\\', "/").to_lowercase().contains(&mod_dir))
        .unwrap_or(false);
    let hook = settings
        .pointer("/hooks/Stop")
        .and_then(Value::as_array)
        .map(|groups| {
            groups
                .iter()
                .any(|g| g.get("hooks").and_then(Value::as_array).map(|l| l.iter().any(is_our_hook)).unwrap_or(false))
        })
        .unwrap_or(false);
    (in_dirs && paths::mod_dir().join("hooks/register.js").exists(), hook)
}

fn valid_session_id(id: &str) -> bool {
    (8..=64).contains(&id.len()) && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// `idlepack hook claude-stop`: Claude Code's Stop hook. Reads the transcript the
/// hook names and records the session's cache facts for the mod. Prints
/// nothing and always exits 0: a Stop hook must never get in a turn's way.
pub fn stop_hook() -> i32 {
    let mut input = String::new();
    let _ = io::stdin().take(4 * 1024 * 1024).read_to_string(&mut input);
    let Ok(event) = serde_json::from_str::<Value>(&input) else { return 0 };
    let Some(id) = event.get("session_id").and_then(Value::as_str).filter(|id| valid_session_id(id)) else { return 0 };
    let Some(transcript) = event.get("transcript_path").and_then(Value::as_str) else { return 0 };
    // Claude Code may run this hook a few milliseconds before it has written the
    // turn's last message to the transcript. The hook is handed that message's
    // text, so wait (briefly) until the file holds it.
    let expected = event
        .get("last_assistant_message")
        .and_then(Value::as_str)
        .map(|t| t.trim().chars().take(200).collect::<String>())
        .filter(|t| !t.is_empty());
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2_500);
    let mut facts = transcript::read_facts(Path::new(transcript));
    while let Some(want) = &expected {
        let arrived = facts.last_text.as_deref().map(|t| t.trim().starts_with(want.as_str())).unwrap_or(false);
        if arrived || std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        facts = transcript::read_facts(Path::new(transcript));
    }
    let record = json!({
        "session_id": id,
        "transcript_path": transcript,
        "cwd": event.get("cwd").cloned().unwrap_or(Value::Null),
        "last_call_ms": facts.last_call_ms,
        "ttl_s": facts.ttl_s,
        "context_tokens": facts.context_tokens,
        "model": facts.model,
        "written_ms": timeutil::now_ms(),
    });
    let _ = fsutil::write_json(&paths::claude_sessions_dir().join(format!("{id}.json")), &record);
    0
}

/// One line per session seen in the last day, for `idlepack status`.
pub fn session_lines(now: i64) -> Vec<String> {
    let mut rows: Vec<(i64, String)> = Vec::new();
    let Ok(entries) = fs::read_dir(paths::claude_sessions_dir()) else { return vec![] };
    for entry in entries.flatten() {
        let Some(facts) = fsutil::read_json_lossy(&entry.path()) else { continue };
        let Some(id) = facts.get("session_id").and_then(Value::as_str) else { continue };
        let written = facts.get("written_ms").and_then(Value::as_i64).unwrap_or(0);
        if now - written > 24 * 3_600_000 {
            continue;
        }
        let live = fsutil::read_json_lossy(&paths::claude_live_dir().join(format!("{id}.json")));
        let ctx = facts
            .get("context_tokens")
            .and_then(Value::as_i64)
            .map(timeutil::human_tokens)
            .unwrap_or_else(|| "?".into());
        let ttl = match facts.get("ttl_s").and_then(Value::as_i64) {
            Some(3_600) => "1h cache".to_owned(),
            Some(300) => "5m cache".to_owned(),
            Some(s) => format!("{s}s cache"),
            None => "cache ?".to_owned(),
        };
        let last_call = facts.get("last_call_ms").and_then(Value::as_i64).unwrap_or(written);
        let mut state = format!("idle {}", timeutil::human_duration(now - last_call));
        match &live {
            None => state.push_str(" · mod not seen (not loaded in this session?)"),
            Some(live) => {
                let beat = live.get("beat_ms").and_then(Value::as_i64).unwrap_or(0);
                let ended = live.get("ended").and_then(Value::as_bool).unwrap_or(false);
                let headless = live.get("headless").and_then(Value::as_bool).unwrap_or(false);
                let where_ = live.get("surface").and_then(Value::as_str).unwrap_or(if headless {
                    "desktop app / SDK"
                } else {
                    "terminal"
                });
                // The threshold as the mod worked it out for this session's model window.
                let threshold = live.pointer("/armed/threshold_tokens").and_then(Value::as_i64);
                let context = facts.get("context_tokens").and_then(Value::as_i64);
                let small = matches!((context, threshold), (Some(c), Some(t)) if c < t);
                if small {
                    state.push_str(&format!(
                        " · under the {} threshold: left alone",
                        timeutil::human_tokens(threshold.unwrap_or_default())
                    ));
                } else if let Some(fire) = live.pointer("/armed/fire_at_ms").and_then(Value::as_i64) {
                    if fire > now {
                        state.push_str(&format!(" · compacts in {}", timeutil::human_duration(fire - now)));
                        if let Some(t) = threshold {
                            state.push_str(&format!(" (over the {} threshold)", timeutil::human_tokens(t)));
                        }
                    }
                }
                if let Some(outcome) = live.pointer("/last/outcome").and_then(Value::as_str) {
                    let reason = live.pointer("/last/reason").and_then(Value::as_str).unwrap_or("");
                    let at = live.pointer("/last/at_ms").and_then(Value::as_i64).unwrap_or(now);
                    state.push_str(&format!(
                        " · last: {outcome}{}{} ago",
                        if reason.is_empty() { " ".into() } else { format!(" ({reason}) ") },
                        timeutil::human_duration(now - at)
                    ));
                }
                if ended {
                    state.push_str(" · session ended");
                } else if now - beat > 3 * 60_000 {
                    state.push_str(&format!(
                        " · {where_} process gone {} ago (the Desktop app may have paused it)",
                        timeutil::human_duration(now - beat)
                    ));
                } else {
                    state.push_str(&format!(" · {where_}"));
                }
            }
        }
        rows.push((last_call, format!("{}  {:>6} ctx  {:<8}  {}", &id[..8.min(id.len())], ctx, ttl, state)));
    }
    rows.sort_by_key(|row| std::cmp::Reverse(row.0));
    rows.into_iter().take(12).map(|(_, s)| s).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_keeps_everything_else() {
        let mut s = json!({
            "model": "opus",
            "env": { "FOO": "1", PLUGIN_DIRS: "/other/mod" },
            "hooks": { "Stop": [ { "hooks": [ { "type": "command", "command": "say done" } ] } ],
                       "PreToolUse": [ { "matcher": "Bash", "hooks": [ { "type": "command", "command": "rtk hook" } ] } ] }
        });
        let original = s.clone();
        let exe = Path::new("/home/me/.cargo/bin/idlepack");
        let change = add_to_settings(&mut s, "/home/me/.idlepack/claude-mod", exe);
        assert!(change.added_plugin_dir && change.added_stop_hook && change.added_function_hooks);
        let sep = list_separator();
        assert_eq!(s["env"][PLUGIN_DIRS], json!(format!("/other/mod{sep}/home/me/.idlepack/claude-mod")));
        assert_eq!(s["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert_eq!(
            s["hooks"]["Stop"][1]["hooks"][0]["command"],
            json!("/home/me/.cargo/bin/idlepack hook claude-stop")
        );

        // A second init changes nothing.
        let again = add_to_settings(&mut s, "/home/me/.idlepack/claude-mod", exe);
        assert!(!again.added_plugin_dir && !again.added_stop_hook && !again.replaced_stop_hook);

        assert!(remove_from_settings(&mut s, "/home/me/.idlepack/claude-mod", true));
        assert_eq!(s, original);
    }

    #[test]
    fn fresh_settings_install_and_remove_cleanly() {
        let mut s = json!({});
        let (installed, spelled_differently) = if cfg!(windows) {
            ("C:/Users/me/.idlepack/claude-mod", "C:\\Users\\me\\.idlepack\\claude-mod\\")
        } else {
            ("/home/me/.idlepack/claude-mod", "/home/me/.idlepack/claude-mod/")
        };
        add_to_settings(&mut s, installed, Path::new("/home/me/.cargo/bin/idlepack"));
        assert!(remove_from_settings(&mut s, spelled_differently, true));
        assert_eq!(s, json!({}));
    }

    #[test]
    fn a_moved_binary_updates_the_hook_in_place() {
        let mut s = json!({});
        add_to_settings(&mut s, "/m", Path::new("/old/idlepack"));
        let change = add_to_settings(&mut s, "/m", Path::new("/new/idlepack"));
        assert!(change.replaced_stop_hook && !change.added_stop_hook);
        assert_eq!(s["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert_eq!(s["hooks"]["Stop"][0]["hooks"][0]["command"], json!("/new/idlepack hook claude-stop"));
    }

    #[test]
    fn paths_with_spaces_are_quoted_for_the_shell_that_runs_them() {
        let (cmd, shell) = hook_command(Path::new("/Users/Jack Smith/bin/idlepack"));
        if cfg!(windows) {
            assert_eq!(shell, Some("powershell"));
            assert!(cmd.starts_with("& '"));
        } else {
            assert_eq!(cmd, "'/Users/Jack Smith/bin/idlepack' hook claude-stop");
            assert_eq!(shell, None);
        }
        let (cmd, shell) = hook_command(Path::new("C:\\Users\\jack\\.cargo\\bin\\idlepack.exe"));
        assert_eq!(cmd, "C:/Users/jack/.cargo/bin/idlepack.exe hook claude-stop");
        assert_eq!(shell, None);
    }

    #[test]
    fn session_ids_are_checked_before_they_name_a_file() {
        assert!(valid_session_id("3f6c2a7e-41d4-4b8e-9a51-2c0d9e1b7a10"));
        assert!(!valid_session_id("../../etc/passwd"));
        assert!(!valid_session_id("short"));
    }
}
