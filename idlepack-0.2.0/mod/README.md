# idlepack (Claude Code mod)

Idle compaction for every Claude Code session this mod loads in, including the
Claude Desktop app's Code tab, where Claude Code's own idle compaction does not run.

After each turn it waits until 90% of the session's one-hour prompt cache lifetime
has passed. If the session is still idle, nothing has compacted it since, and its
context is big enough (by default half the model's window, at most 200k tokens), it
compacts the session while the cache is still warm, so coming back after the cache
expired re-reads a short summary instead of the whole conversation.

It is installed and configured by the `idlepack` command (`idlepack init`), which
also adds a small Stop hook that records each session's cache lifetime from its
transcript. Settings live in `~/.idlepack/config.json` under `"claude"`.
