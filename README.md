# idlepack

[![ci](https://github.com/JackRozells/RIC-RustIdleCompacting/actions/workflows/release.yml/badge.svg)](https://github.com/JackRozells/RIC-RustIdleCompacting/actions/workflows/release.yml)
[![crates.io](https://img.shields.io/crates/v/idlepack.svg)](https://crates.io/crates/idlepack)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**RIC: Rust Idle Compacting.** Idle compaction for the **Claude** and **Codex
desktop apps**, as one Rust binary you install like [rtk](https://github.com/rtk-ai/rtk).

When a long conversation sits idle past its prompt-cache lifetime, the next
message re-sends the whole context uncached. Claude Code 2.1.299 fixed that in
the terminal: a big session left idle for ~54 minutes is compacted just before
its one-hour cache expires, so you come back to a short summary instead of
paying to re-read everything. The Claude Desktop app's Code tab doesn't get
that, and Codex doesn't have it at all. idlepack does the same job in both.

```
turn ends ──► wait until 90% of the cache lifetime has passed
          ──► still idle? nothing compacted it since? context big enough?
          ──► compact now, inside the app, while the cache is still warm
```

## Install

One line, macOS / Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/JackRozells/RIC-RustIdleCompacting/main/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/JackRozells/RIC-RustIdleCompacting/main/install.ps1 | iex
```

The installer downloads the prebuilt binary for your platform from the
[latest release](https://github.com/JackRozells/RIC-RustIdleCompacting/releases/latest)
(checking its SHA-256), or builds from source with cargo if there isn't one,
then runs `idlepack init`. Building from source needs Rust; see
[rustup.rs](https://rustup.rs).

From [crates.io](https://crates.io/crates/idlepack), on any platform with Rust:

```sh
cargo install idlepack
idlepack init
```

From a checkout:

```sh
git clone https://github.com/JackRozells/RIC-RustIdleCompacting.git
cd RIC-RustIdleCompacting
sh install.sh                                          # macOS / Linux
powershell -ExecutionPolicy Bypass -File install.ps1   # Windows
```

Or by hand: `cargo install --path .` then `idlepack init`.

Then **fully quit and restart** the Claude desktop app and the Codex desktop app
so they pick it up.

Installer variables: `IDLEPACK_BIN_DIR` (where the binary goes; default
`~/.local/bin`, or `%LOCALAPPDATA%\idlepack\bin` on Windows),
`IDLEPACK_FROM_SOURCE=1` (always build with cargo), `IDLEPACK_NO_INIT=1` (install
the binary only), `IDLEPACK_REPO` (install from a fork).

## Commands

| Command | What it does |
|---|---|
| `idlepack init [--claude] [--codex] [--min-tokens SIZE]` | Installs both halves (or one), optionally setting the threshold. Safe to re-run. |
| `idlepack status` | What's installed, and what each live session/thread is waiting for. |
| `idlepack gain [--days N]` | Compactions made, context before → after, what wasn't re-cached. |
| `idlepack config` / `config set KEY VALUE` | Show or change settings (`~/.idlepack/config.json`), e.g. `config set minTokens 150k`. |
| `idlepack uninstall [--claude] [--codex] [--purge]` | Takes out exactly what `init` put in. |

## How it works

### Claude Code (Desktop Code tab, and the terminal)

- **A Claude Code mod** (a plugin with a hooks module, Claude Code ≥ 2.1.284)
  runs inside every session. `init` writes it to `~/.idlepack/claude-mod` and
  names that folder in `CLAUDE_CODE_PLUGIN_DIRS` in `~/.claude/settings.json`,
  the one way to load a mod into the Desktop app.
- After each turn it plans a compaction at **90% of the cache lifetime** (54 min
  on the 1-hour cache, matching Claude Code's own). If the session is still idle
  then, nothing has compacted it, and its context is at least the threshold, it
  compacts **in-process**: through `/compact` in Desktop/SDK sessions (where
  Claude Code doesn't let a mod call the operation directly) and through the
  operation itself in the terminal. Because it's the session's own compaction,
  the request reads the conversation from the warm cache.
- **A Stop hook** (`idlepack hook claude-stop`) reads the session transcript at
  each turn's end and records the cache lifetime Claude Code actually bought
  (5-minute or 1-hour), so the mod knows when to fire. 5-minute caches are
  skipped by default (compacting every coffee break costs more than it saves).
- In a **terminal** session Claude Code may already do this itself, so the mod
  fires a little later (92%) and stands down if anything compacted first.

### Codex desktop

- The Codex desktop app finds its app-server through the `CODEX_CLI_PATH`
  environment variable before its bundled binary. `init` points that at a copy
  of idlepack (`~/.idlepack/shim/codex[.exe]`), which runs the real bundled
  `codex` and passes its stdio through, byte for byte, a line at a time.
- Watching that stream (`turn/started`, `turn/completed`,
  `thread/tokenUsage/updated`), it keeps an idle timer per thread. When a
  thread has been idle for **27 minutes** (90% of a 30-minute cache lifetime)
  and is big enough, it writes one `thread/compact/start` request into the
  app-server's stdin under its own id, and takes the reply out of the stream so
  the app never sees an answer to a request it didn't send. The compaction runs
  in the app-server that holds the thread, and shows in the app as a normal
  compaction.
- Anything that isn't the stdio app-server (`codex --version`, `codex exec`…) is
  handed straight to the real binary.

## Settings

### How big a session has to be

Idle compaction only touches a session whose context is at least the
**threshold**. Set it for both apps at once, or for one:

```sh
idlepack config set minTokens 150k          # both apps
idlepack config set claude.minTokens 300k   # Claude only
idlepack config set codex.minTokens 40%     # Codex: 40% of the model's context window
idlepack config set minTokens auto          # back to the default
idlepack init --min-tokens 150k             # or at install time
```

A threshold is a token count (`150000`, `150k`, `1.5m`), a share of the model's
window (`40%`, kept between 5% and 95%), or `auto`: half the window, at most
200k (so 200k on a 1M-token model, 100k on a 200k one). Nothing below 20k is
used, and under ~60k a compaction saves little, since the system prompt and
tools stay in every request (about 45k in Claude Code). `idlepack status` shows
the threshold in force and, for each session, whether it is over it.

### Everything else

`idlepack config set KEY VALUE`. The mod re-reads settings at each turn's end;
the Codex shim within a minute.

| Key | Default | Meaning |
|---|---|---|
| `claude.enabled` / `codex.enabled` | `true` | Switch either half off without uninstalling. |
| `claude.minTokens` / `codex.minTokens` | `"auto"` | The threshold (above). `minTokens` alone sets both. |
| `claude.fireAtFraction` | `0.9` | When to fire, as a fraction of the cache lifetime. |
| `claude.terminal` | `"auto"` | `auto`: fire after Claude Code's own idle compaction would; `off`: leave terminals to Claude Code; `on`. |
| `claude.allowFiveMinuteCache` | `false` | Also compact sessions on the 5-minute cache (at 4.5 min). |
| `claude.instructions` | `""` | Text passed after `/compact` (what the summary should keep). Your CLAUDE.md `# Compact instructions` section applies either way. |
| `claude.toast` | `true` | Show a toast after each compaction. |
| `codex.cacheTtlMinutes` | `30` | Assumed Codex cache lifetime (Codex doesn't report it; see below). |
| `codex.fireAtFraction` | `0.9` | As above, for Codex. |
| `codex.realCodexPath` | `null` | Pin the real `codex` the shim runs (else auto-detected). |

## Checking it works

1. `idlepack status` right after a turn should list the session with its context
   size and `1h cache`, and (once the mod's heartbeat lands) `compacts in 53m`.
   `mod not seen` means the mod didn't load in that session (see limits below).
2. Leave a big session idle for an hour, then `idlepack gain`.
3. Logs: `~/.idlepack/log/codex-shim.log` (metadata only, never conversation text).

## Limits and caveats

- **Compaction loses detail.** You come back to a summary, exactly as with
  Claude Code's own idle compaction. Turn a half off if a project needs full
  context, or steer the summary with `claude.instructions` / CLAUDE.md.
- **It only pays off if you return after the cache expired.** Compacting reads
  the context from cache (cheap) and writes a summary; if you come back while
  the cache is still warm, it cost a little and dropped detail.
- **Claude Desktop may stop idle background sessions.** A 2026 bug report
  ([anthropics/claude-code#68625](https://github.com/anthropics/claude-code/issues/68625))
  found Desktop (Windows, an older build) kills a session's process after ~15
  minutes idle while its tab is hidden. If your build still does that, the
  mod's timer goes with the process and that session isn't compacted;
  `idlepack status` shows `process gone`. Nothing outside the app can stop it.
- **Desktop sessions running in WSL don't load plugins**, so the mod can't
  work there (it does in native Windows/macOS sessions).
- **Mods are an early-access Claude Code API** (on by default from 2.1.287;
  `init` also sets `CLAUDE_CODE_ENABLE_FUNCTION_HOOKS=1` for 2.1.284–2.1.286).
  The Desktop app bundles its own Claude Code; keep it updated.
- **Codex's cache lifetime is assumed.** Codex records how much was cached, not
  for how long. OpenAI documents a 30-minute (sliding) default for GPT-5.6 and
  later, and conPACT's measurement across 108 Codex rollouts found cache hits
  reliable up to ~45 minutes of idle. Raise `codex.cacheTtlMinutes` to compact
  later, lower it to be safer.
- **The Codex shim depends on `CODEX_CLI_PATH`.** If a Codex update stops
  honoring it, the shim is simply bypassed. On macOS it's set with `launchctl`
  plus a login agent; on Windows in your user environment (HKCU); on Linux in
  `~/.config/environment.d`.
- Not affiliated with Anthropic or OpenAI.

## Uninstall

```sh
idlepack uninstall          # add --purge to delete ~/.idlepack too
cargo uninstall idlepack    # or delete the binary
```

`uninstall` restores `~/.claude/settings.json` to what it was (your other
settings untouched; a backup of the original is in
`settings.json.idlepack-backup`) and puts `CODEX_CLI_PATH` back to whatever it
was before.

## Development

| Path | What's there |
|---|---|
| `src/` | The `idlepack` binary: CLI, config, Claude Stop hook, and the Codex shim (`src/codex/`). |
| `mod/` | The Claude Code mod `init` installs (hooks module, policy, and its tests). |
| `tests/` | Integration tests: CLI round trip, and the shim against a fake app-server. |
| `examples/fake_app_server.rs` | A stand-in Codex app-server for trying the shim by hand. |
| `install.sh`, `install.ps1` | The installers. |

```sh
cargo clippy --all-targets -- -D warnings
cargo test                     # unit tests, CLI round trip, the shim against a fake app-server
claude plugin test mod         # the mod, against Claude Code's own test host (Claude Code CLI ≥ 2.1.284)
claude plugin validate mod
```

To try the shim by hand:
`IDLEPACK_CODEX_REAL=target/debug/examples/fake_app_server idlepack __codex-shim app-server`.

CI runs clippy and the tests on Linux, macOS and Windows for every push and pull
request. Pushing a version tag (`git tag v0.2.0 && git push origin v0.2.0`)
also builds release binaries for Linux x86_64, macOS (Apple Silicon and Intel)
and Windows x86_64 and attaches them, with checksums, to a GitHub release, which
is what the one-line installers download.

## Contributing

Issues and pull requests are welcome. Please run `cargo fmt`, clippy and
`cargo test` before opening a PR, and include the Claude Code / Codex app
versions and your OS when reporting a bug (`idlepack status` output helps).

## Prior art

Claude Code's own idle compaction (terminal only, per-account rollout).
[conPACT](https://github.com/st0nebridge/conPACT) (Python, MIT) compacts idle
Claude Code Desktop and ChatGPT Desktop/Codex sessions behind an ask-first toast;
its research showed the two routes idlepack takes — a mod running `/compact` in
the Desktop Code tab, and a `CODEX_CLI_PATH` shim around the Codex app-server.
idlepack is an independent, automatic, single-binary take on the same idea.

## License

[MIT](LICENSE).
