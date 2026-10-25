//! One persistent user environment variable that apps started from the desktop
//! (not from a shell) will see: `CODEX_CLI_PATH`, which the Codex desktop app
//! reads to find its app-server.
//!
//! - Windows: the user's registry environment (`HKCU\Environment`), written
//!   through .NET's `[Environment]::SetEnvironmentVariable(..., 'User')`, which
//!   also tells running programs that the environment changed.
//! - macOS: `launchctl setenv` for the current login session, and a per-user
//!   LaunchAgent that sets it again at each login.
//! - Linux: `~/.config/environment.d/60-idlepack.conf`, which a systemd
//!   graphical session imports at login (plus `systemctl --user
//!   set-environment` for the session running now, when there is one).
//!
//! Every backend is user scope and reversible, and the app picks the change up
//! the next time it starts.

use crate::paths;
use std::io;
#[cfg(not(windows))]
use std::path::PathBuf;
use std::process::Command;

#[cfg(target_os = "macos")]
const AGENT_LABEL: &str = "dev.idlepack.codex-cli-path";

fn run(cmd: &mut Command) -> io::Result<String> {
    let out = cmd.output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        return Err(io::Error::other(if err.is_empty() { format!("{cmd:?} failed") } else { err }));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

// ------------------------------------------------------------------ Windows --

#[cfg(windows)]
fn powershell(script: &str) -> io::Result<String> {
    run(Command::new("powershell.exe").args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        script,
    ]))
}

#[cfg(windows)]
fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(windows)]
pub fn get(name: &str) -> io::Result<Option<String>> {
    let v = powershell(&format!("[Environment]::GetEnvironmentVariable({}, 'User')", ps_quote(name)))?;
    Ok(Some(v).filter(|v| !v.is_empty()))
}

#[cfg(windows)]
pub fn set(name: &str, value: &str) -> io::Result<()> {
    powershell(&format!("[Environment]::SetEnvironmentVariable({}, {}, 'User')", ps_quote(name), ps_quote(value)))
        .map(|_| ())
}

#[cfg(windows)]
pub fn clear(name: &str) -> io::Result<()> {
    powershell(&format!("[Environment]::SetEnvironmentVariable({}, $null, 'User')", ps_quote(name))).map(|_| ())
}

#[cfg(windows)]
pub fn how_it_applies() -> &'static str {
    "written to your user environment; fully quit and restart the Codex app to apply"
}

// -------------------------------------------------------------------- macOS --

#[cfg(target_os = "macos")]
fn agent_path() -> PathBuf {
    paths::home_dir().join("Library/LaunchAgents").join(format!("{AGENT_LABEL}.plist"))
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[cfg(target_os = "macos")]
pub fn get(name: &str) -> io::Result<Option<String>> {
    let live = run(Command::new("/bin/launchctl").args(["getenv", name])).unwrap_or_default();
    if !live.is_empty() {
        return Ok(Some(live));
    }
    // Not live (logged in since, before the agent ran?): the agent's record.
    let text = std::fs::read_to_string(agent_path()).unwrap_or_default();
    let marker = format!("<string>{}</string>", xml_escape(name));
    if let Some(at) = text.find(&marker) {
        let rest = &text[at + marker.len()..];
        if let (Some(start), Some(end)) = (rest.find("<string>"), rest.find("</string>")) {
            if start < end {
                let value = rest[start + 8..end]
                    .replace("&quot;", "\"")
                    .replace("&gt;", ">")
                    .replace("&lt;", "<")
                    .replace("&amp;", "&");
                return Ok(Some(value).filter(|v| !v.is_empty()));
            }
        }
    }
    Ok(None)
}

#[cfg(target_os = "macos")]
pub fn set(name: &str, value: &str) -> io::Result<()> {
    run(Command::new("/bin/launchctl").args(["setenv", name, value]))?;
    if let Ok(uid) = run(Command::new("/usr/bin/id").arg("-u")) {
        let _ = run(Command::new("/bin/launchctl").args(["asuser", &uid, "/bin/launchctl", "setenv", name, value]));
    }
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/launchctl</string>
    <string>setenv</string>
    <string>{name}</string>
    <string>{value}</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
</dict>
</plist>
"#,
        label = AGENT_LABEL,
        name = xml_escape(name),
        value = xml_escape(value)
    );
    crate::fsutil::write_atomic(&agent_path(), plist.as_bytes())
}

#[cfg(target_os = "macos")]
pub fn clear(name: &str) -> io::Result<()> {
    let _ = run(Command::new("/bin/launchctl").args(["unsetenv", name]));
    if let Ok(uid) = run(Command::new("/usr/bin/id").arg("-u")) {
        let _ = run(Command::new("/bin/launchctl").args(["asuser", &uid, "/bin/launchctl", "unsetenv", name]));
    }
    match std::fs::remove_file(agent_path()) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(target_os = "macos")]
pub fn how_it_applies() -> &'static str {
    "set with launchctl and a login agent; quit and restart the Codex app (Cmd+Q) to apply"
}

// -------------------------------------------------------------------- Linux --

#[cfg(all(unix, not(target_os = "macos")))]
fn envd_path() -> PathBuf {
    paths::home_dir().join(".config/environment.d/60-idlepack.conf")
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn get(name: &str) -> io::Result<Option<String>> {
    let text = match std::fs::read_to_string(envd_path()) {
        Ok(t) => t,
        // Not ours to keep: whatever the session this runs in already has.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(std::env::var(name).ok().filter(|v| !v.is_empty())),
        Err(e) => return Err(e),
    };
    for line in text.lines() {
        if let Some((key, value)) = line.trim().split_once('=') {
            if key.trim() == name {
                let v = value.trim().trim_matches('"').to_owned();
                return Ok(Some(v).filter(|v| !v.is_empty()));
            }
        }
    }
    Ok(None)
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn set(name: &str, value: &str) -> io::Result<()> {
    if value.contains('"') || value.contains('\n') {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "a path with a quote or newline cannot be stored"));
    }
    let body =
        format!("# Written by idlepack: the Codex desktop app runs its app-server through this.\n{name}=\"{value}\"\n");
    crate::fsutil::write_atomic(&envd_path(), body.as_bytes())?;
    let _ = run(Command::new("systemctl").args(["--user", "set-environment", &format!("{name}={value}")]));
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn clear(name: &str) -> io::Result<()> {
    let _ = run(Command::new("systemctl").args(["--user", "unset-environment", name]));
    match std::fs::remove_file(envd_path()) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn how_it_applies() -> &'static str {
    "written to ~/.config/environment.d; log out and back in (or restart the app from a session that has it) to apply"
}

#[cfg(not(any(unix, windows)))]
compile_error!("idlepack supports Windows, macOS and Linux");

/// For messages: where the value is kept on this platform.
pub fn store_name() -> String {
    if cfg!(windows) {
        "HKCU\\Environment".into()
    } else if cfg!(target_os = "macos") {
        "launchctl + ~/Library/LaunchAgents".into()
    } else {
        paths::pretty(&paths::home_dir().join(".config/environment.d/60-idlepack.conf"))
    }
}
