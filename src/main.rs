//! idlepack: idle compaction for the Claude and Codex desktop apps.
//!
//! When a big conversation sits idle, its prompt cache expires, and the next
//! message re-reads the whole context at full price. Claude Code's terminal
//! compacts such a session shortly before the cache expires; the desktop apps
//! do not. idlepack does it for both:
//!
//! - Claude Code (terminal and the Desktop app's Code tab): a mod that runs
//!   inside the session and compacts it in-process, plus a Stop hook that
//!   records each session's cache lifetime.
//! - Codex desktop: a shim the app runs as its app-server, which asks that
//!   app-server to compact a thread that has gone idle.
//!
//! `idlepack init` installs both; `idlepack status`, `idlepack gain`,
//! `idlepack config` and `idlepack uninstall` do what they say.

mod claude;
mod codex;
mod config;
mod events;
mod fsutil;
mod paths;
mod timeutil;
mod transcript;
mod userenv;

use clap::{Parser, Subcommand};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "idlepack",
    version,
    about = "Idle compaction for the Claude and Codex desktop apps",
    long_about = "Compacts a big idle Claude Code or Codex session shortly before its prompt cache expires, \
                  so coming back to it re-reads a short summary instead of the whole conversation. \
                  Works in the Claude Desktop app's Code tab and the Codex desktop app."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install for Claude Code and the Codex desktop app (both, unless one is named)
    Init {
        /// Only Claude Code (terminal and the Desktop app's Code tab)
        #[arg(long)]
        claude: bool,
        /// Only the Codex desktop app
        #[arg(long)]
        codex: bool,
        /// Only compact sessions at least this big: 150000, 150k, 1.5m, 40% (of the model's window) or auto
        #[arg(long, value_name = "SIZE")]
        min_tokens: Option<String>,
    },
    /// What is installed, and what each live session or thread is waiting for
    Status,
    /// What the idle compactions came to
    Gain {
        /// How far back to look, in days
        #[arg(long, default_value_t = 30)]
        days: i64,
    },
    /// Show or change settings (~/.idlepack/config.json)
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },
    /// Take out everything init put in (both, unless one is named)
    Uninstall {
        #[arg(long)]
        claude: bool,
        #[arg(long)]
        codex: bool,
        /// Also delete ~/.idlepack (settings, events, logs)
        #[arg(long)]
        purge: bool,
    },
    /// Hooks the installed integrations call (not for direct use)
    #[command(hide = true)]
    Hook {
        #[command(subcommand)]
        which: HookKind,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print the settings in force (the default)
    Show,
    /// Set one: `idlepack config set claude.minTokens 150000`
    Set { key: String, value: String },
    /// Print where the config file is
    Path,
}

#[derive(Subcommand)]
enum HookKind {
    /// Claude Code's Stop hook: records the session's cache lifetime for the mod
    ClaudeStop,
}

fn heading(text: &str) {
    println!("\n{text}");
}

fn bullet(lines: &[String]) {
    for line in lines {
        println!("  {line}");
    }
}

/// This binary's path, as the Stop hook records it. A package manager that keeps
/// each version in a folder of its own (Homebrew's Cellar) links the binary onto
/// the PATH, and the link outlives an upgrade where the versioned path does not,
/// so an entry on the PATH that is this same file wins over the resolved path.
fn this_exe() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|_| "idlepack".into());
    let real = std::fs::canonicalize(&exe).map(strip_verbatim).unwrap_or(exe);
    link_on_path(&real, std::env::var_os("PATH")).unwrap_or(real)
}

/// The first entry on `path_var` with this binary's name that is the same file as `real`.
fn link_on_path(real: &Path, path_var: Option<OsString>) -> Option<PathBuf> {
    let name = real.file_name()?;
    let target = std::fs::canonicalize(real).ok()?;
    std::env::split_paths(&path_var?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| std::fs::canonicalize(candidate).map(|c| c == target).unwrap_or(false))
}

/// `\\?\C:\...` (what canonicalize gives on Windows) back to `C:\...`.
fn strip_verbatim(p: PathBuf) -> PathBuf {
    let text = p.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => p,
    }
}

fn init(claude_only: bool, codex_only: bool, min_tokens: Option<String>) -> Result<(), String> {
    let both = !claude_only && !codex_only;
    let exe = this_exe();
    println!("idlepack {} · state in {}", env!("CARGO_PKG_VERSION"), paths::pretty(&paths::state_dir()));
    if config::ensure_file().map_err(|e| e.to_string())? {
        println!("wrote default settings to {}", paths::pretty(&paths::config_file()));
    }
    if let Some(size) = min_tokens {
        // The threshold goes to the apps being installed; both when neither is named.
        let key = match (claude_only, codex_only) {
            (true, false) => "claude.minTokens",
            (false, true) => "codex.minTokens",
            _ => "minTokens",
        };
        report_set(&config::set(key, &size).map_err(|e| e.to_string())?);
    }
    let mut failures = Vec::new();
    if both || claude_only {
        heading("Claude Code");
        match claude::install(&exe) {
            Ok(report) => bullet(&report.lines),
            Err(err) => {
                println!("  failed: {err}");
                failures.push("Claude Code");
            }
        }
    }
    if both || codex_only {
        heading("Codex desktop");
        match codex::install(&exe) {
            Ok(lines) => bullet(&lines),
            Err(err) => {
                println!("  failed: {err}");
                failures.push("Codex");
            }
        }
    }
    heading("Next");
    if both || claude_only {
        println!("  · Quit and reopen the Claude desktop app; new Code-tab and terminal sessions load the mod.");
        println!("    (Mods need Claude Code 2.1.284 or later; the desktop app bundles its own and updates it.)");
    }
    if both || codex_only {
        println!(
            "  · Quit the Codex desktop app completely and start it again, so it runs its app-server through the shim."
        );
    }
    println!("  · `idlepack status` shows what each session is waiting for; `idlepack gain` what it came to.");
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!("{} could not be installed (see above)", failures.join(" and ")))
    }
}

fn status() {
    let now = timeutil::now_ms();
    println!("idlepack {} · state in {}", env!("CARGO_PKG_VERSION"), paths::pretty(&paths::state_dir()));
    heading("Claude Code");
    let (mod_loaded, hook) = claude::installed_state();
    println!(
        "  mod             {}",
        if mod_loaded {
            format!("{} via CLAUDE_CODE_PLUGIN_DIRS ✓", paths::pretty(&paths::mod_dir()))
        } else {
            "not installed (run `idlepack init --claude`)".into()
        }
    );
    println!("  Stop hook       {}", if hook { "installed ✓" } else { "not installed" });
    println!("  settings        {}", paths::pretty(&claude::settings_path()));
    let cfg = config::load();
    let fraction = cfg.pointer("/claude/fireAtFraction").and_then(|v| v.as_f64()).unwrap_or(0.9);
    println!(
        "  timing          compacts after {} idle on a 1h cache (Desktop); terminal sessions defer to Claude Code's own",
        timeutil::human_duration((3_600_000.0 * fraction) as i64)
    );
    println!(
        "  threshold       {}  (claude.minTokens; `idlepack config set minTokens 150k` sets both apps)",
        config::threshold_for(&cfg, "claude").describe()
    );
    let sessions = claude::session_lines(now);
    if sessions.is_empty() {
        println!("  sessions        none in the last 24h (the Stop hook records each one at its turn's end)");
    } else {
        println!("  sessions (last 24h)");
        for line in sessions {
            println!("    {line}");
        }
    }
    heading("Codex desktop");
    bullet(&codex::status_lines(now));
}

fn uninstall(claude_only: bool, codex_only: bool, purge: bool) -> Result<(), String> {
    let both = !claude_only && !codex_only;
    let mut failed = false;
    if both || claude_only {
        heading("Claude Code");
        match claude::uninstall() {
            Ok(lines) if lines.is_empty() => println!("  nothing to remove"),
            Ok(lines) => bullet(&lines),
            Err(err) => {
                println!("  failed: {err}");
                failed = true;
            }
        }
    }
    if both || codex_only {
        heading("Codex desktop");
        match codex::uninstall() {
            Ok(lines) if lines.is_empty() => println!("  nothing to remove"),
            Ok(lines) => bullet(&lines),
            Err(err) => {
                println!("  failed: {err}");
                failed = true;
            }
        }
    }
    if purge && both && !failed {
        let dir = paths::state_dir();
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => println!("\ndeleted {}", paths::pretty(&dir)),
            Err(err) => println!("\ncould not delete {}: {err}", paths::pretty(&dir)),
        }
    }
    println!("\nRestart the desktop apps for the change to take effect.");
    if failed {
        Err("some parts could not be removed (see above)".into())
    } else {
        Ok(())
    }
}

fn report_set(stored: &config::Stored) {
    for key in &stored.keys {
        print!("{key} = {}", stored.value);
        if key.ends_with("minTokens") {
            if let Some(t) = config::Threshold::parse(&stored.value) {
                print!("  ({})", t.describe());
            }
        }
        println!();
    }
    for note in &stored.notes {
        println!("  note: {note}");
    }
}

fn config_cmd(action: Option<ConfigAction>) -> Result<(), String> {
    match action.unwrap_or(ConfigAction::Show) {
        ConfigAction::Show => {
            println!("# {}", paths::config_file().display());
            println!("{}", serde_json::to_string_pretty(&config::load()).map_err(|e| e.to_string())?);
            println!("# claude.* is read by the mod at each turn's end; codex.* by the shim within a minute.");
        }
        ConfigAction::Set { key, value } => report_set(&config::set(&key, &value).map_err(|e| e.to_string())?),
        ConfigAction::Path => println!("{}", paths::config_file().display()),
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    // Run by the Codex desktop app as `codex ...`: be the shim, nothing else.
    if codex::shim::is_shim_invocation(args.first()) {
        return ExitCode::from(codex::shim::run(args[1..].to_vec()).clamp(0, 255) as u8);
    }
    if args.get(1).map(|a| a == "__codex-shim").unwrap_or(false) {
        return ExitCode::from(codex::shim::run(args[2..].to_vec()).clamp(0, 255) as u8);
    }
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Init { claude, codex, min_tokens } => init(claude, codex, min_tokens),
        Command::Status => {
            status();
            Ok(())
        }
        Command::Gain { days } => {
            print!("{}", events::report(days.max(1)));
            Ok(())
        }
        Command::Config { action } => config_cmd(action),
        Command::Uninstall { claude, codex, purge } => uninstall(claude, codex, purge),
        Command::Hook { which: HookKind::ClaudeStop } => return ExitCode::from(claude::stop_hook() as u8),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("idlepack: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn a_link_on_the_path_wins_over_the_versioned_path() {
        let root = std::env::temp_dir().join(format!("idlepack-test-{}-cellar", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let cellar = root.join("Cellar/idlepack/0.2.0/bin");
        let bin = root.join("bin");
        let other = root.join("other");
        for dir in [&cellar, &bin, &other] {
            fs::create_dir_all(dir).unwrap();
        }
        let real = cellar.join("idlepack");
        fs::write(&real, "real").unwrap();
        fs::write(other.join("idlepack"), "some other idlepack").unwrap();
        std::os::unix::fs::symlink(&real, bin.join("idlepack")).unwrap();

        let path = std::env::join_paths([&other, &bin]).unwrap();
        assert_eq!(link_on_path(&real, Some(path)), Some(bin.join("idlepack")));
        // Nothing on the PATH is this binary, or there is no PATH: keep the resolved path.
        assert_eq!(link_on_path(&real, Some(std::env::join_paths([&other]).unwrap())), None);
        assert_eq!(link_on_path(&real, Some("relative/bin".into())), None);
        assert_eq!(link_on_path(&real, None), None);
        let _ = fs::remove_dir_all(&root);
    }
}
