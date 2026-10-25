//! Where everything lives. idlepack keeps its own files under `~/.idlepack`
//! (or `$IDLEPACK_HOME`), and touches only two things outside it: Claude
//! Code's user settings, and the `CODEX_CLI_PATH` user environment variable.

use std::env;
use std::path::PathBuf;

/// The user's home: `%USERPROFILE%` on Windows, `$HOME` elsewhere. The Claude
/// mod resolves it the same way, so both sides agree on `~/.idlepack`.
pub fn home_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(profile) = env::var_os("USERPROFILE").filter(|p| !p.is_empty()) {
            return PathBuf::from(profile);
        }
    }
    if let Some(home) = env::var_os("HOME").filter(|p| !p.is_empty()) {
        return PathBuf::from(home);
    }
    #[cfg(windows)]
    {
        if let (Some(drive), Some(path)) = (env::var_os("HOMEDRIVE"), env::var_os("HOMEPATH")) {
            let mut p = PathBuf::from(drive);
            p.push(path);
            return p;
        }
    }
    PathBuf::from(".")
}

/// idlepack's own folder.
pub fn state_dir() -> PathBuf {
    match env::var_os("IDLEPACK_HOME").filter(|p| !p.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => home_dir().join(".idlepack"),
    }
}

/// Claude Code's user configuration folder (`CLAUDE_CONFIG_DIR` or `~/.claude`).
pub fn claude_dir() -> PathBuf {
    match env::var_os("CLAUDE_CONFIG_DIR").filter(|p| !p.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => home_dir().join(".claude"),
    }
}

pub fn config_file() -> PathBuf {
    state_dir().join("config.json")
}

/// What `init` changed, so `uninstall` can put it back exactly.
pub fn install_record() -> PathBuf {
    state_dir().join("install.json")
}

pub fn mod_dir() -> PathBuf {
    state_dir().join("claude-mod")
}

pub fn claude_sessions_dir() -> PathBuf {
    state_dir().join("claude").join("sessions")
}

pub fn claude_live_dir() -> PathBuf {
    state_dir().join("claude").join("live")
}

pub fn codex_live_dir() -> PathBuf {
    state_dir().join("codex")
}

pub fn events_dir() -> PathBuf {
    state_dir().join("events")
}

pub fn log_dir() -> PathBuf {
    state_dir().join("log")
}

pub fn shim_dir() -> PathBuf {
    state_dir().join("shim")
}

/// The executable the Codex desktop app runs instead of `codex`: a copy of this
/// very binary, named so it knows to act as the shim.
pub fn shim_exe() -> PathBuf {
    shim_dir().join(if cfg!(windows) { "codex.exe" } else { "codex" })
}

/// Shows a path with the home folder as `~`, for messages.
pub fn pretty(path: &std::path::Path) -> String {
    let home = home_dir();
    match path.strip_prefix(&home) {
        Ok(rest) if !rest.as_os_str().is_empty() => format!("~/{}", rest.to_string_lossy().replace('\\', "/")),
        _ => path.display().to_string(),
    }
}
