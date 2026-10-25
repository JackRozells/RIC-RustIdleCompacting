/**
 * @module idlepack.policy
 * @description The decisions, with nothing of Claude Code in them: what the
 *              settings mean, when a timer should fire, whether a session is
 *              worth compacting and whether its cache is still warm. register.js
 *              asks; this answers. Kept pure so a test can read every rule.
 */

export const VERSION = '0.2.0'

export const HOUR_MS = 3_600_000
export const FIVE_MIN_MS = 300_000

/** How long after a turn ends the mod reads the Stop hook's cache facts. */
export const PROBE_DELAY_MS = 20_000
/** A timer this late has lapsed (the machine slept): Claude Code's own rule. */
export const LATE_MS = 60_000
/** Never start a compaction this close to the cache's expiry (or 5% of a shorter lifetime). */
export const EXPIRY_MARGIN_MS = 30_000

export function expiryMargin(ttlMs) {
  return Math.min(EXPIRY_MARGIN_MS, Math.round(ttlMs * 0.05))
}
/** How often a live session writes its heartbeat for `idlepack status`. */
export const BEAT_MS = 60_000
/** A Stop-hook record older than the turn end by more than this is stale. */
export const FACTS_SLACK_MS = 120_000

/** The smallest threshold used (below it a compaction saves next to nothing), and the most `auto` asks for. */
export const FLOOR_TOKENS = 20_000
export const AUTO_CAP_TOKENS = 200_000
/** A rate-limit window this full is left alone: compacting spends tokens too. */
export const RATE_LIMIT_PERCENT = 95

export const DEFAULTS = Object.freeze({
  enabled: true,
  // How big the context must be before the session is compacted: "auto" (half the
  // model's window, at most 200k), a token count (150000, "150k", "1.5m"), or a share
  // of the window ("40%"). `idlepack config set minTokens 150k` writes it.
  minTokens: 'auto',
  // Of the cache lifetime, measured from the turn's last request. Claude Code's own uses 0.9.
  fireAtFraction: 0.9,
  // In a terminal Claude Code may compact idle sessions itself; "auto" fires a little
  // later than it would, and stands down if anything compacted the session first.
  terminal: 'auto',
  terminalFireAtFraction: 0.92,
  // A 5-minute cache is gone after a coffee; compacting that often costs more than it saves.
  allowFiveMinuteCache: false,
  // Passed to the compaction, as text after /compact; empty keeps Claude Code's own prompt
  // (and the "# Compact instructions" section of CLAUDE.md).
  instructions: '',
  toast: true,
})

/** The settings under "claude" in ~/.idlepack/config.json, over the defaults, each checked. */
export function settings(raw) {
  const given = raw && typeof raw === 'object' && raw.claude && typeof raw.claude === 'object' ? raw.claude : {}
  const s = { ...DEFAULTS }
  if (typeof given.enabled === 'boolean') s.enabled = given.enabled
  s.minTokens = parseThreshold(given.minTokens) ?? AUTO
  if (Number.isFinite(given.fireAtFraction)) s.fireAtFraction = clamp(given.fireAtFraction, 0.5, 0.95)
  if (['auto', 'on', 'off'].includes(given.terminal)) s.terminal = given.terminal
  if (Number.isFinite(given.terminalFireAtFraction)) s.terminalFireAtFraction = clamp(given.terminalFireAtFraction, 0.5, 0.95)
  if (typeof given.allowFiveMinuteCache === 'boolean') s.allowFiveMinuteCache = given.allowFiveMinuteCache
  if (typeof given.instructions === 'string') s.instructions = given.instructions.slice(0, 2000)
  if (typeof given.toast === 'boolean') s.toast = given.toast
  return s
}

function clamp(value, low, high) {
  return Math.min(high, Math.max(low, value))
}

const AUTO = Object.freeze({ kind: 'auto' })

/**
 * A threshold as written in the config, read the way the idlepack command reads it:
 * "auto", a token count (150000, "150000", "150,000", "150k", "1.5m"), or a share of
 * the model's window ("40%", kept between 5% and 95%). Null for anything else.
 */
export function parseThreshold(value) {
  if (typeof value === 'number') return Number.isFinite(value) && value > 0 ? { kind: 'tokens', n: Math.round(value) } : null
  if (typeof value !== 'string') return null
  const t = value.trim().toLowerCase().replace(/[,_ ]/g, '')
  if (t === 'auto') return AUTO
  const number = (text) => (/^\d+(\.\d+)?$|^\.\d+$/.test(text) ? Number(text) : NaN)
  if (t.endsWith('%')) {
    const p = number(t.slice(0, -1))
    return Number.isFinite(p) && p > 0 && p <= 100 ? { kind: 'percent', p: clamp(p, 5, 95) } : null
  }
  const scale = t.endsWith('k') ? 1_000 : t.endsWith('m') ? 1_000_000 : 1
  const n = number(scale === 1 ? t : t.slice(0, -1)) * scale
  return Number.isFinite(n) && n > 0 ? { kind: 'tokens', n: Math.round(n) } : null
}

/** The context size below which a session is not worth compacting, for this model's window. */
export function threshold(s, windowTokens) {
  const window = Number.isFinite(windowTokens) && windowTokens > 0 ? windowTokens : null
  const auto = () => Math.max(FLOOR_TOKENS, Math.min(AUTO_CAP_TOKENS, window === null ? AUTO_CAP_TOKENS : Math.floor(window / 2)))
  const t = s.minTokens ?? AUTO
  if (t.kind === 'tokens') return Math.max(FLOOR_TOKENS, t.n)
  if (t.kind === 'percent') return window === null ? auto() : Math.max(FLOOR_TOKENS, Math.round((window * t.p) / 100))
  return auto()
}

/** "1h" / "5m" (Claude Code's spellings) as milliseconds, or null. */
export function parseTtl(value) {
  if (value === '1h') return HOUR_MS
  if (value === '5m') return FIVE_MIN_MS
  return null
}

/**
 * What the Stop hook recorded for this session, if it describes the turn that just
 * ended: `{ ttlMs, lastCallAt, contextTokens }`, or null when absent, malformed or stale.
 */
export function factsFromProbe(record, sessionId, turnEndAt) {
  if (!record || typeof record !== 'object' || record.session_id !== sessionId) return null
  const ttlMs = Number.isFinite(record.ttl_s) && record.ttl_s > 0 ? record.ttl_s * 1000 : null
  const lastCallAt = Number.isFinite(record.last_call_ms) ? record.last_call_ms : null
  const written = Number.isFinite(record.written_ms) ? record.written_ms : null
  if (ttlMs === null || lastCallAt === null || written === null) return null
  // Written for an earlier turn: the hook has not run for this one (yet).
  if (written < turnEndAt - FACTS_SLACK_MS) return null
  const contextTokens = Number.isFinite(record.context_tokens) ? record.context_tokens : null
  return { ttlMs, lastCallAt, contextTokens, source: 'stop-hook' }
}

/**
 * The cache lifetime Claude Code most likely chose, when the Stop hook has not said:
 * its own order of precedence (forced 5m, the env override, the promptCacheTtl
 * setting, the 1h opt-in), then 1h for subscribers (who have rate-limit windows)
 * and 5m for everyone else.
 */
export function inferTtl({ force5m, envTtl, settingTtl, enable1h, isSubscriber }) {
  if (force5m) return FIVE_MIN_MS
  const fromEnv = parseTtl(envTtl)
  if (fromEnv !== null) return fromEnv
  const fromSetting = parseTtl(settingTtl)
  if (fromSetting !== null) return fromSetting
  if (enable1h) return HOUR_MS
  return isSubscriber ? HOUR_MS : FIVE_MIN_MS
}

/**
 * When to compact: a fraction of the cache lifetime after the turn's last request
 * (or the turn's end, when the request's time is unknown, whichever is earlier).
 */
export function fireAt({ turnEndAt, lastCallAt, ttlMs, fraction }) {
  const anchor = Number.isFinite(lastCallAt) ? Math.min(lastCallAt, turnEndAt) : turnEndAt
  return Math.round(anchor + ttlMs * fraction)
}

/** The fraction that applies to a session: terminal sessions defer to Claude Code's own timer. */
export function fractionFor(s, headless) {
  return headless ? s.fireAtFraction : s.terminalFireAtFraction
}

/** Whether a session of this kind is the mod's to compact at all. */
export function handles(s, headless) {
  if (!s.enabled) return false
  return headless || s.terminal !== 'off'
}

/**
 * The reason not to compact now, or null to go ahead. `armed` is what was planned at
 * the turn end; `facts` the cache facts as they stand now; `now` the clock.
 */
export function reasonToSkip({ s, armed, facts, now, tokens, windowTokens, compactedAt, rateLimits }) {
  if (compactedAt > armed.turnEndAt) return 'already compacted since the turn ended'
  if (now > armed.fireAt + LATE_MS) return 'lapsed: the timer fired late (the computer may have slept)'
  if (facts && Number.isFinite(facts.lastCallAt) && facts.lastCallAt > armed.turnEndAt + 5_000) {
    return 'a newer request has been made since the turn ended'
  }
  const anchor = facts && Number.isFinite(facts.lastCallAt) ? Math.min(facts.lastCallAt, armed.turnEndAt) : armed.turnEndAt
  if (anchor + armed.ttlMs - expiryMargin(armed.ttlMs) <= now) return 'the prompt cache has already expired'
  const min = threshold(s, windowTokens)
  if (!Number.isFinite(tokens)) return 'the context size is unknown'
  if (tokens < min) return `small: ${formatTokens(tokens)} is under the ${formatTokens(min)} threshold`
  if (Array.isArray(rateLimits) && rateLimits.some((r) => Number.isFinite(r?.percentUsed) && r.percentUsed >= RATE_LIMIT_PERCENT)) {
    return 'a rate-limit window is nearly full'
  }
  return null
}

/** 412880 -> "413k", 1250000 -> "1.25M". */
export function formatTokens(n) {
  if (!Number.isFinite(n)) return '?'
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2).replace(/\.?0+$/, '')}M`
  if (n >= 1_000) return `${Math.round(n / 1_000)}k`
  return String(Math.round(n))
}

/** A user's home and idlepack's folder in it, with forward slashes everywhere. */
export function stateDir({ idlepackHome, userProfile, home }) {
  const tidy = (p) => p.replace(/\\/g, '/').replace(/\/+$/, '')
  if (idlepackHome) return tidy(idlepackHome)
  // A Windows profile is the home there; elsewhere HOME is.
  if (userProfile && /^[A-Za-z]:[\\/]/.test(userProfile)) return `${tidy(userProfile)}/.idlepack`
  if (home) return `${tidy(home)}/.idlepack`
  return null
}

/** Session ids are UUIDs; anything else is not used in a path. */
export function isSessionId(id) {
  return typeof id === 'string' && /^[0-9a-fA-F-]{8,64}$/.test(id)
}
