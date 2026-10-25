//! Finding the real `codex` the desktop app would have run without the shim.
//!
//! The Codex desktop app resolves its app-server through `CODEX_CLI_PATH`
//! before the binary it bundles, which is what lets the shim stand in. The
//! shim then has to find that bundled binary itself. In order: an explicit pin
//! (`IDLEPACK_CODEX_REAL`, then `codex.realCodexPath` in the config), whatever
//! `CODEX_CLI_PATH` pointed at before idlepack took it over, the newest codex an
//! installed app bundles, and last `codex` on the PATH. Never the shim itself.
//!
//! Bundle locations follow what conPACT read from the desktop app's own code:
//! `%LOCALAPPDATA%\OpenAI\Codex\bin\<hash>\codex.exe` and the app's resources on
//! Windows, `Contents/Resources/codex` inside an app bundle on macOS, and
//! `resources/codex` under `/opt`, `/usr/lib` or `~/.local/share` on Linux. The
//! newest by modification time wins, because the Windows folders are content
//! hashes that carry no order.

use crate::{config::CodexSettings, fsutil, paths};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const REAL_ENV: &str = "IDLEPACK_CODEX_REAL";

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// True for the shim, and for any copy of this binary.
pub fn is_ours(candidate: &Path) -> bool {
    if same_file(candidate, &paths::shim_exe()) {
        return true;
    }
    if let Ok(me) = env::current_exe() {
        if same_file(candidate, &me) {
            return true;
        }
    }
    candidate.starts_with(paths::shim_dir())
}

fn usable(p: &Path) -> bool {
    p.is_file() && !is_ours(p)
}

fn modified(p: &Path) -> SystemTime {
    fs::metadata(p).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH)
}

fn children(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir).map(|it| it.flatten().map(|e| e.path()).collect()).unwrap_or_default()
}

/// Every codex an installed desktop app bundles, as far as this platform has them.
pub fn bundled_candidates() -> Vec<PathBuf> {
    let mut found = Vec::new();
    let exe = if cfg!(windows) { "codex.exe" } else { "codex" };
    if cfg!(target_os = "macos") {
        for root in [PathBuf::from("/Applications"), paths::home_dir().join("Applications")] {
            for app in children(&root) {
                if app.extension().map(|e| e == "app").unwrap_or(false) {
                    found.push(app.join("Contents").join("Resources").join(exe));
                }
            }
        }
    } else if cfg!(windows) {
        if let Some(local) = env::var_os("LOCALAPPDATA").map(PathBuf::from) {
            for hashed in children(&local.join("OpenAI").join("Codex").join("bin")) {
                found.push(hashed.join(exe));
            }
            for app in children(&local.join("Programs")) {
                found.push(app.join("resources").join(exe));
            }
        }
        if let Some(program_files) = env::var_os("ProgramFiles").map(PathBuf::from) {
            // Store installs; often not listable, which simply finds nothing.
            for pkg in children(&program_files.join("WindowsApps")) {
                let name = pkg.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
                if name.contains("openai") || name.contains("codex") || name.contains("chatgpt") {
                    found.push(pkg.join("app").join("resources").join(exe));
                    found.push(pkg.join("resources").join(exe));
                }
            }
        }
    } else {
        for root in [PathBuf::from("/opt"), PathBuf::from("/usr/lib"), paths::home_dir().join(".local/share")] {
            for app in children(&root) {
                found.push(app.join("resources").join(exe));
            }
        }
    }
    found.retain(|p| usable(p));
    found.sort_by_key(|p| std::cmp::Reverse(modified(p)));
    found
}

fn on_path() -> Option<PathBuf> {
    let names: &[&str] = if cfg!(windows) { &["codex.exe", "codex.cmd", "codex"] } else { &["codex"] };
    for dir in env::split_paths(&env::var_os("PATH")?) {
        for name in names {
            let candidate = dir.join(name);
            if usable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// Where the real codex is, and how it was found.
pub fn real_codex(settings: &CodexSettings) -> Option<(PathBuf, &'static str)> {
    if let Some(pin) = env::var_os(REAL_ENV).map(PathBuf::from).filter(|p| usable(p)) {
        return Some((pin, "IDLEPACK_CODEX_REAL"));
    }
    if let Some(pin) = settings.real_codex_path.as_ref().map(PathBuf::from).filter(|p| usable(p)) {
        return Some((pin, "codex.realCodexPath in the config"));
    }
    if let Some(previous) = fsutil::read_json_lossy(&paths::install_record())
        .and_then(|r| r.pointer("/codex/previousCliPath").and_then(|v| v.as_str()).map(PathBuf::from))
        .filter(|p| usable(p))
    {
        return Some((previous, "CODEX_CLI_PATH before idlepack"));
    }
    if let Some(bundled) = bundled_candidates().into_iter().next() {
        return Some((bundled, "the newest codex an installed app bundles"));
    }
    on_path().map(|p| (p, "codex on the PATH"))
}
