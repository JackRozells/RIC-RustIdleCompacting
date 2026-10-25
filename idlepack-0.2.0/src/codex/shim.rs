//! The program the Codex desktop app runs instead of `codex`.
//!
//! The desktop app talks to its app-server over stdio, and the thread worth
//! compacting is the one that app-server holds open: only that process can
//! compact it, and it has no socket of its own. So the shim runs the real
//! codex with the arguments it was given and stands in its stdio path:
//!
//!   desktop --stdin-->  shim  --stdin-->  codex app-server
//!   desktop <-stdout--  shim  <-stdout--  codex app-server
//!
//! Every byte is passed on as it came, whole lines at a time. Watching the
//! lines, the shim knows when each thread's turn ends, how big its context is,
//! and when someone uses it again. When a thread has been idle for 90% of the
//! cache lifetime and is big enough, the shim writes one `thread/compact/start`
//! request into the app-server's stdin, between the desktop's own lines, under
//! an id of its own; the reply to that id is taken out of the stream, so the
//! desktop never sees an answer to a request it did not send. The compaction
//! itself runs in the app-server that holds the thread, with the prefix it has
//! cached, and shows in the desktop app as a compaction of that thread.
//!
//! Anything that is not the stdio app-server (`codex --version`, `codex exec`,
//! a listener) is handed to the real codex untouched. The shim fails open:
//! no state folder, no log, a status file it cannot write, none of it stops
//! the app-server or the desktop app.

use super::locate;
use super::proto::{self, ServerEvent};
use super::watcher::{Action, Watcher};
use crate::config::CodexSettings;
use crate::{fsutil, paths, timeutil};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const CLI_ENV: &str = "CODEX_CLI_PATH";
const STATUS_EVERY_MS: i64 = 30_000;
const SETTINGS_EVERY_MS: i64 = 60_000;
const LOG_MAX_BYTES: u64 = 1024 * 1024;

/// Whether this process was started as the shim: by the name it was run under.
pub fn is_shim_invocation(argv0: Option<&OsString>) -> bool {
    let Some(argv0) = argv0 else { return false };
    let text = argv0.to_string_lossy();
    // Either separator: Windows passes backslashes, and either may reach any build.
    let name = text.rsplit(['/', '\\']).next().unwrap_or("").to_lowercase();
    name == "codex" || name == "codex.exe"
}

struct Shared {
    child_in: Mutex<Option<ChildStdin>>,
    watcher: Mutex<Watcher>,
    done: AtomicBool,
    pid: u32,
    child_pid: u32,
    real: PathBuf,
    started_ms: i64,
}

fn log(line: &str) {
    let stamped = format!("{} pid={} {line}", timeutil::format_rfc3339(timeutil::now_ms()), std::process::id());
    fsutil::append_log(&paths::log_dir().join("codex-shim.log"), &stamped, LOG_MAX_BYTES);
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

/// Runs the shim with the arguments after the program name. Returns the exit code.
pub fn run(args: Vec<OsString>) -> i32 {
    let settings = CodexSettings::load();
    let Some((real, how)) = locate::real_codex(&settings) else {
        eprintln!(
            "idlepack: could not find the real codex to run (set {} to its path, or codex.realCodexPath in {})",
            locate::REAL_ENV,
            paths::config_file().display()
        );
        log("no real codex found; nothing run");
        return 127;
    };
    let text_args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    if !proto::is_stdio_app_server(&text_args) {
        return passthrough(&real, &args);
    }
    match proxy(&real, &args, settings) {
        Ok(code) => code,
        Err(err) => {
            log(&format!("could not start {} ({how}): {err}", real.display()));
            eprintln!("idlepack: could not start {}: {err}", real.display());
            1
        }
    }
}

fn child_command(real: &Path, args: &[OsString]) -> Command {
    let mut cmd = Command::new(real);
    cmd.args(args);
    // Codex looks itself up through this when it starts helpers: point it at itself,
    // not back at the shim.
    cmd.env(CLI_ENV, real);
    #[cfg(windows)]
    {
        // Started by a desktop app, the shim has no console of its own, and a console
        // program it starts would get a new, visible one. Its output goes through pipes.
        use std::io::IsTerminal;
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        if !io::stdout().is_terminal() {
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
    }
    cmd
}

/// Anything but the stdio app-server: become the real codex.
fn passthrough(real: &Path, args: &[OsString]) -> i32 {
    let mut cmd = child_command(real, args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        eprintln!("idlepack: could not run {}: {err}", real.display());
        127
    }
    #[cfg(not(unix))]
    {
        match cmd.status() {
            Ok(status) => status.code().unwrap_or(1),
            Err(err) => {
                eprintln!("idlepack: could not run {}: {err}", real.display());
                127
            }
        }
    }
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

fn proxy(real: &Path, args: &[OsString], settings: CodexSettings) -> io::Result<i32> {
    let mut child: Child =
        child_command(real, args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn()?;
    let child_in = child.stdin.take();
    let child_out = child.stdout.take().expect("piped");
    let pid = std::process::id();
    let shared = Arc::new(Shared {
        child_in: Mutex::new(child_in),
        watcher: Mutex::new(Watcher::new(settings, format!("idlepack-{pid}-"))),
        done: AtomicBool::new(false),
        pid,
        child_pid: child.id(),
        real: real.to_path_buf(),
        started_ms: timeutil::now_ms(),
    });
    log(&format!("proxying app-server {} (child pid {})", real.display(), child.id()));

    {
        let shared = Arc::clone(&shared);
        thread::spawn(move || pump_from_desktop(io::stdin(), &shared));
    }
    let (out_done, out_finished) = mpsc::channel();
    {
        let shared = Arc::clone(&shared);
        thread::spawn(move || {
            pump_to_desktop(child_out, io::stdout(), &shared);
            let _ = out_done.send(());
        });
    }
    {
        let shared = Arc::clone(&shared);
        thread::spawn(move || ticker(&shared));
    }

    let status = child.wait()?;
    // Let the last of the child's output through, but never hang on a pipe a
    // grandchild still holds open.
    let _ = out_finished.recv_timeout(Duration::from_secs(3));
    shared.done.store(true, Ordering::SeqCst);
    let _ = std::fs::remove_file(status_path(pid));
    log(&format!("app-server exited ({status})"));
    Ok(exit_code(status))
}

fn write_to_child(shared: &Shared, line: &[u8]) -> bool {
    let mut guard = shared.child_in.lock().unwrap_or_else(|p| p.into_inner());
    match guard.as_mut() {
        Some(stdin) => stdin.write_all(line).and_then(|_| stdin.flush()).is_ok(),
        None => false,
    }
}

/// Desktop -> app-server. A request that means someone is using a thread
/// disarms it before the line goes on, so an idle compaction never lands
/// behind a turn the person just started.
fn pump_from_desktop<R: Read>(input: R, shared: &Shared) {
    let mut reader = BufReader::with_capacity(64 * 1024, input);
    let mut line = Vec::with_capacity(64 * 1024);
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if let Some(event) = proto::client_event(&line) {
            let mut w = shared.watcher.lock().unwrap_or_else(|p| p.into_inner());
            w.on_client(event, timeutil::now_ms());
        }
        if !write_to_child(shared, &line) {
            break;
        }
    }
    // The desktop closed its side: so do we, and the app-server ends.
    shared.child_in.lock().unwrap_or_else(|p| p.into_inner()).take();
}

/// App-server -> desktop, minus the replies to our own requests.
fn pump_to_desktop<R: Read, W: Write>(output: R, mut desktop: W, shared: &Shared) {
    let mut reader = BufReader::with_capacity(64 * 1024, output);
    let mut line = Vec::with_capacity(64 * 1024);
    let tag = shared.watcher.lock().unwrap_or_else(|p| p.into_inner()).tag().to_owned();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let event = proto::server_event(&line, &tag);
        let ours = matches!(event, Some(ServerEvent::OurReply { .. }));
        if !ours && desktop.write_all(&line).and_then(|_| desktop.flush()).is_err() {
            break;
        }
        if let Some(event) = event {
            let describe = match &event {
                ServerEvent::OurReply { id, error } => Some(format!(
                    "compaction request {id} {}",
                    error.as_ref().map(|e| format!("refused: {e}")).unwrap_or_else(|| "accepted".into())
                )),
                _ => None,
            };
            let actions = {
                let mut w = shared.watcher.lock().unwrap_or_else(|p| p.into_inner());
                w.on_server(event, timeutil::now_ms())
            };
            if let Some(text) = describe {
                log(&text);
            }
            perform(shared, actions);
        }
    }
    let _ = desktop.flush();
}

fn perform(shared: &Shared, actions: Vec<Action>) {
    for action in actions {
        match action {
            Action::Inject { thread, line } => {
                let sent = write_to_child(shared, &line);
                log(&format!(
                    "thread {} idle: compaction {}",
                    short(&thread),
                    if sent { "requested" } else { "could not be sent" }
                ));
            }
            Action::Record(mut event) => {
                if let Some(map) = event.as_object_mut() {
                    map.insert("shim_version".into(), json!(env!("CARGO_PKG_VERSION")));
                }
                let at = event.get("at_ms").and_then(Value::as_i64).unwrap_or_else(timeutil::now_ms);
                let thread = event.get("thread_id").and_then(Value::as_str).unwrap_or("thread").to_owned();
                let outcome = event.get("outcome").and_then(Value::as_str).unwrap_or("?").to_owned();
                let before = event.get("tokens_before").and_then(Value::as_i64);
                let after = event.get("tokens_after").and_then(Value::as_i64);
                log(&format!(
                    "thread {} {outcome}{}",
                    short(&thread),
                    match (before, after) {
                        (Some(b), Some(a)) =>
                            format!(" {} -> {}", timeutil::human_tokens(b), timeutil::human_tokens(a)),
                        (Some(b), None) => format!(" from {}", timeutil::human_tokens(b)),
                        _ => String::new(),
                    }
                ));
                let name = format!("codex-{at}-{}.json", short(&thread));
                let _ = fsutil::write_json(&paths::events_dir().join(name), &event);
            }
        }
    }
}

fn status_path(pid: u32) -> PathBuf {
    paths::codex_live_dir().join(format!("shim-{pid}.json"))
}

fn write_status(shared: &Shared) {
    let threads = shared.watcher.lock().unwrap_or_else(|p| p.into_inner()).snapshot();
    let status = json!({
        "pid": shared.pid,
        "child_pid": shared.child_pid,
        "real_codex": shared.real.to_string_lossy(),
        "started_ms": shared.started_ms,
        "beat_ms": timeutil::now_ms(),
        "version": env!("CARGO_PKG_VERSION"),
        "threads": threads,
    });
    let _ = fsutil::write_json(&status_path(shared.pid), &status);
}

fn ticker(shared: &Shared) {
    let mut last_status = 0i64;
    let mut last_settings = timeutil::now_ms();
    loop {
        let tick = shared.watcher.lock().unwrap_or_else(|p| p.into_inner()).settings.tick_ms;
        thread::sleep(Duration::from_millis(tick));
        if shared.done.load(Ordering::SeqCst) {
            return;
        }
        let now = timeutil::now_ms();
        if now - last_settings >= SETTINGS_EVERY_MS {
            last_settings = now;
            let fresh = CodexSettings::load();
            shared.watcher.lock().unwrap_or_else(|p| p.into_inner()).settings = fresh;
        }
        let actions = shared.watcher.lock().unwrap_or_else(|p| p.into_inner()).tick(now);
        perform(shared, actions);
        if now - last_status >= STATUS_EVERY_MS {
            last_status = now;
            write_status(shared);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knows_its_names() {
        assert!(is_shim_invocation(Some(&OsString::from("/home/me/.idlepack/shim/codex"))));
        assert!(is_shim_invocation(Some(&OsString::from("C:\\Users\\me\\.idlepack\\shim\\codex.exe"))));
        assert!(is_shim_invocation(Some(&OsString::from("CODEX.EXE"))));
        assert!(!is_shim_invocation(Some(&OsString::from("/usr/local/bin/idlepack"))));
        assert!(!is_shim_invocation(None));
    }
}
