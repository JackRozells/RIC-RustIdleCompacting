//! `idlepack init`, `status`, the Stop hook and `uninstall`, run as a user
//! would, against a temporary home folder (Linux and macOS layout; the
//! environment variable goes to ~/.config/environment.d on Linux).
#![cfg(all(unix, not(target_os = "macos")))]

use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("idlepack-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn idlepack(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_idlepack"))
        .args(args)
        .env("HOME", home)
        .env_remove("IDLEPACK_HOME")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("IDLEPACK_CODEX_REAL")
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap()
}

fn text(out: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

fn read(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn init_status_hook_and_uninstall_leave_the_settings_as_they_were() {
    let home = scratch("roundtrip");
    let settings_path = home.join(".claude/settings.json");
    std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
    let original = json!({
        "model": "opus",
        "env": { "MY_VAR": "1" },
        "hooks": { "PreToolUse": [ { "matcher": "Bash", "hooks": [ { "type": "command", "command": "rtk hook" } ] } ] },
        "permissions": { "allow": ["Bash(git status)"] }
    });
    std::fs::write(&settings_path, serde_json::to_string_pretty(&original).unwrap()).unwrap();

    let out = idlepack(&home, &["init"]);
    assert!(out.status.success(), "{}", text(&out));
    let report = text(&out);
    assert!(report.contains("wrote the Claude Code mod"), "{report}");
    assert!(report.contains("placed the Codex shim"), "{report}");

    // The mod, the settings, the shim, the variable.
    let mod_dir = home.join(".idlepack/claude-mod");
    for file in [".claude-plugin/plugin.json", "hooks/hooks.json", "hooks/register.js", "hooks/policy.js"] {
        assert!(mod_dir.join(file).is_file(), "{file} written");
    }
    let settings = read(&settings_path);
    assert_eq!(settings["env"]["CLAUDE_CODE_PLUGIN_DIRS"], json!(mod_dir.to_string_lossy()));
    assert_eq!(settings["env"]["MY_VAR"], "1");
    assert_eq!(settings["model"], "opus");
    let stop = settings["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap().to_owned();
    assert!(stop.ends_with(" hook claude-stop"), "{stop}");
    assert_eq!(settings["hooks"]["PreToolUse"], original["hooks"]["PreToolUse"]);
    assert!(home.join(".claude/settings.json.idlepack-backup").is_file());
    let shim = home.join(".idlepack/shim/codex");
    assert!(shim.is_file());
    let envd = std::fs::read_to_string(home.join(".config/environment.d/60-idlepack.conf")).unwrap();
    assert!(envd.contains(&format!("CODEX_CLI_PATH=\"{}\"", shim.display())), "{envd}");
    assert!(home.join(".idlepack/config.json").is_file());

    // A second init is a no-op for the settings.
    let again = idlepack(&home, &["init", "--claude"]);
    assert!(again.status.success());
    assert_eq!(read(&settings_path), settings);

    // The Stop hook, as Claude Code runs it: the hook's JSON on stdin.
    let session = "3f6c2a7e-41d4-4b8e-9a51-2c0d9e1b7a10";
    let transcript = home.join("t.jsonl");
    let entry = json!({ "type": "assistant", "timestamp": "2026-10-23T15:04:05.123Z", "message": { "model": "claude-opus-5-5", "usage": {
        "input_tokens": 3, "cache_read_input_tokens": 400_000, "cache_creation_input_tokens": 2_000, "output_tokens": 500,
        "cache_creation": { "ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 2_000 } } } });
    std::fs::write(&transcript, format!("{entry}\n")).unwrap();
    let mut hook = Command::new(env!("CARGO_BIN_EXE_idlepack"))
        .args(["hook", "claude-stop"])
        .env("HOME", &home)
        .env_remove("IDLEPACK_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let event = json!({ "session_id": session, "transcript_path": transcript, "cwd": "/work", "hook_event_name": "Stop", "stop_hook_active": false });
    hook.stdin.take().unwrap().write_all(event.to_string().as_bytes()).unwrap();
    let done = hook.wait_with_output().unwrap();
    assert!(done.status.success());
    assert!(done.stdout.is_empty(), "a Stop hook says nothing");
    let facts = read(&home.join(format!(".idlepack/claude/sessions/{session}.json")));
    assert_eq!(facts["ttl_s"], 3600);
    assert_eq!(facts["context_tokens"], 402_503);
    assert_eq!(facts["last_call_ms"], 1_792_767_845_123i64);

    // A malformed hook input is shrugged off.
    let mut bad = Command::new(env!("CARGO_BIN_EXE_idlepack"))
        .args(["hook", "claude-stop"])
        .env("HOME", &home)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    bad.stdin.take().unwrap().write_all(b"{\"session_id\":\"../../x\"").unwrap();
    assert!(bad.wait().unwrap().success());

    let status = idlepack(&home, &["status"]);
    let status_text = text(&status);
    assert!(status_text.contains("via CLAUDE_CODE_PLUGIN_DIRS ✓"), "{status_text}");
    assert!(status_text.contains("Stop hook       installed ✓"), "{status_text}");
    assert!(status_text.contains("3f6c2a7e"), "{status_text}");
    assert!(status_text.contains("-> the shim ✓"), "{status_text}");

    // config set, and a typo refused.
    assert!(idlepack(&home, &["config", "set", "claude.minTokens", "150000"]).status.success());
    assert_eq!(read(&home.join(".idlepack/config.json"))["claude"]["minTokens"], 150000);
    assert!(!idlepack(&home, &["config", "set", "claude.minTokenz", "1"]).status.success());

    // An event, as the mod writes one, shows in gain.
    std::fs::create_dir_all(home.join(".idlepack/events")).unwrap();
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    std::fs::write(
        home.join(".idlepack/events/claude-1-3f6c2a7e.json"),
        json!({ "app": "claude", "session_id": session, "at_ms": now_ms - 60_000, "outcome": "compacted",
                "tokens_before": 412_880, "tokens_after": 21_300, "idle_ms": 3_238_000 })
        .to_string(),
    )
    .unwrap();
    let gain = text(&idlepack(&home, &["gain"]));
    assert!(gain.contains("Idle compactions   1  (Claude 1, Codex 0)"), "{gain}");
    assert!(gain.contains("413k -> 21k"), "{gain}");

    // Uninstall puts the settings back as they were and removes what init placed.
    let out = idlepack(&home, &["uninstall"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(read(&settings_path), original);
    assert!(!mod_dir.exists());
    assert!(!home.join(".idlepack/shim").exists());
    assert!(!home.join(".config/environment.d/60-idlepack.conf").exists());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn the_stop_hook_runs_the_path_link_not_the_versioned_binary() {
    // As Homebrew installs it: the binary in a versioned folder, a link to it on the PATH.
    let home = scratch("pathlink");
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let link = bin.join("idlepack");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_idlepack"), &link).unwrap();
    let out = Command::new(&link)
        .args(["init", "--claude"])
        .env("HOME", &home)
        .env_remove("IDLEPACK_HOME")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env("PATH", format!("/usr/bin:/bin:{}", bin.display()))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    let settings = read(&home.join(".claude/settings.json"));
    assert_eq!(
        settings["hooks"]["Stop"][0]["hooks"][0]["command"],
        json!(format!("{} hook claude-stop", link.display()))
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_settings_file_that_is_not_json_is_left_alone() {
    let home = scratch("badjson");
    let settings_path = home.join(".claude/settings.json");
    std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
    std::fs::write(&settings_path, "{ \"model\": \"opus\", // a comment\n}").unwrap();
    let out = idlepack(&home, &["init", "--claude"]);
    assert!(!out.status.success());
    assert!(text(&out).contains("left alone"), "{}", text(&out));
    assert_eq!(std::fs::read_to_string(&settings_path).unwrap(), "{ \"model\": \"opus\", // a comment\n}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn an_existing_codex_cli_path_is_kept_and_restored() {
    let home = scratch("prevcli");
    let envd = home.join(".config/environment.d/60-idlepack.conf");
    std::fs::create_dir_all(envd.parent().unwrap()).unwrap();
    // Someone pointed the app at their own codex build before.
    let theirs = home.join("my-codex");
    std::fs::write(&theirs, "#!/bin/sh\n").unwrap();
    std::fs::write(&envd, format!("CODEX_CLI_PATH=\"{}\"\n", theirs.display())).unwrap();
    let out = idlepack(&home, &["init", "--codex"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("uninstall puts it back"), "{}", text(&out));
    assert!(std::fs::read_to_string(&envd).unwrap().contains(".idlepack/shim/codex"));
    let record = read(&home.join(".idlepack/install.json"));
    assert_eq!(record["codex"]["previousCliPath"], json!(theirs.to_string_lossy()));
    let out = idlepack(&home, &["uninstall", "--codex"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(std::fs::read_to_string(&envd).unwrap().contains(&format!("CODEX_CLI_PATH=\"{}\"", theirs.display())));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn the_stop_hook_waits_for_the_turn_to_reach_the_transcript() {
    let home = scratch("race");
    let session = "5a1b2c3d-41d4-4b8e-9a51-2c0d9e1b7a10";
    let transcript = home.join("t.jsonl");
    let old = json!({ "type": "assistant", "timestamp": "2026-10-23T14:00:00Z", "message": { "model": "m",
        "content": [ { "type": "text", "text": "an earlier answer" } ],
        "usage": { "input_tokens": 1, "cache_read_input_tokens": 100, "cache_creation_input_tokens": 0, "output_tokens": 1 } } });
    std::fs::write(&transcript, format!("{old}\n")).unwrap();
    let mut hook = Command::new(env!("CARGO_BIN_EXE_idlepack"))
        .args(["hook", "claude-stop"])
        .env("HOME", &home)
        .env_remove("IDLEPACK_HOME")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let event = json!({ "session_id": session, "transcript_path": transcript, "hook_event_name": "Stop",
                        "last_assistant_message": "All done: the tests pass." });
    hook.stdin.take().unwrap().write_all(event.to_string().as_bytes()).unwrap();
    // Claude Code writes the turn's last message a moment after starting the hook.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let new = json!({ "type": "assistant", "timestamp": "2026-10-23T15:04:05.123Z", "message": { "model": "m",
        "content": [ { "type": "text", "text": "All done: the tests pass." } ],
        "usage": { "input_tokens": 2, "cache_read_input_tokens": 400_000, "cache_creation_input_tokens": 1_000, "output_tokens": 50,
                   "cache_creation": { "ephemeral_1h_input_tokens": 1_000, "ephemeral_5m_input_tokens": 0 } } } });
    let mut f = std::fs::OpenOptions::new().append(true).open(&transcript).unwrap();
    writeln!(f, "{new}").unwrap();
    drop(f);
    assert!(hook.wait().unwrap().success());
    let facts = read(&home.join(format!(".idlepack/claude/sessions/{session}.json")));
    assert_eq!(facts["last_call_ms"], 1_792_767_845_123i64);
    assert_eq!(facts["ttl_s"], 3600);
    assert_eq!(facts["context_tokens"], 401_052);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn the_threshold_is_set_for_both_apps_in_any_spelling() {
    let home = scratch("threshold");
    std::fs::create_dir_all(home.join(".claude")).unwrap();

    // At install time.
    let out = idlepack(&home, &["init", "--claude", "--min-tokens", "300k"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("claude.minTokens = 300000  (300k)"), "{}", text(&out));
    let config = read(&home.join(".idlepack/config.json"));
    assert_eq!(config["claude"]["minTokens"], 300_000);
    assert_eq!(config["codex"]["minTokens"], "auto", "only the app being installed");

    // Later, for both apps at once, in a person's spelling.
    let out = idlepack(&home, &["config", "set", "minTokens", "150k"]);
    assert!(out.status.success(), "{}", text(&out));
    let config = read(&home.join(".idlepack/config.json"));
    assert_eq!(config["claude"]["minTokens"], 150_000);
    assert_eq!(config["codex"]["minTokens"], 150_000);

    // A share of the window, for one app.
    let out = idlepack(&home, &["config", "set", "codex.minTokens", "40%"]);
    assert!(text(&out).contains("40% of the model's window"), "{}", text(&out));
    assert_eq!(read(&home.join(".idlepack/config.json"))["codex"]["minTokens"], "40%");

    // Small values are kept but flagged; tiny ones are raised to the floor.
    let out = idlepack(&home, &["config", "set", "claude.minTokens", "40k"]);
    assert!(text(&out).contains("note: below ~60k"), "{}", text(&out));
    let out = idlepack(&home, &["config", "set", "claude.minTokens", "5000"]);
    assert!(text(&out).contains("raised to the 20k minimum"), "{}", text(&out));
    assert_eq!(read(&home.join(".idlepack/config.json"))["claude"]["minTokens"], 20_000);

    // Nonsense is refused and changes nothing.
    let out = idlepack(&home, &["config", "set", "minTokens", "lots"]);
    assert!(!out.status.success());
    assert!(text(&out).contains("is not a threshold"), "{}", text(&out));
    assert_eq!(read(&home.join(".idlepack/config.json"))["codex"]["minTokens"], "40%");

    // Status says what is in force.
    let status = text(&idlepack(&home, &["status"]));
    assert!(status.contains("threshold       20k  (claude.minTokens"), "{status}");
    assert!(status.contains("threshold       40% of the model's window  (codex.minTokens)"), "{status}");
    let _ = std::fs::remove_dir_all(&home);
}
