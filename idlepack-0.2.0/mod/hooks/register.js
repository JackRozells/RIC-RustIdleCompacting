/**
 * @module idlepack.register
 * @description Idle compaction for every Claude Code session this mod loads in,
 *              the Desktop app's Code tab included.
 *
 *              Claude Code compacts a big idle session shortly before its
 *              one-hour prompt cache expires, but only in a terminal, where a
 *              compaction host exists. This mod does the same job from inside the
 *              session:
 *
 *                turn ends -> plan: read the cache lifetime the Stop hook recorded
 *                          -> wait until 90% of it has passed
 *                          -> still idle, nothing compacted since, big enough?
 *                          -> compact now, while the cache is warm
 *
 *              The compaction is the session's own: $.session.compact where Claude
 *              Code lets a mod call it, and /compact through $.command.run where it
 *              refuses (a session with no terminal: the Desktop app's Code tab, the
 *              Agent SDK). Both reach the operation /compact runs.
 *
 *              Any new turn disarms the timer; so does any compaction, Claude
 *              Code's own idle compaction included. Each live session writes a
 *              heartbeat for `idlepack status`, and each compaction it makes an
 *              event for `idlepack gain`. It starts no process and writes only
 *              under idlepack's own folder.
 * @input      Claude Code's events: session.start, turn.start, turn.complete,
 *              session.compact, session.end; ~/.idlepack/config.json and the
 *              Stop hook's ~/.idlepack/claude/sessions/<id>.json
 * @output     compactions; ~/.idlepack/claude/live/<id>.json heartbeats and
 *              ~/.idlepack/events/*.json records; a toast per compaction
 */
import * as policy from './policy.js'

// Module state: a hot reload starts it over, which the store and the heartbeat survive.
let headless = false
let surface = null
let generation = 0
let armed = null          // { token, turnEndAt, fireAt, ttlMs, source, timers }
let compactedAt = 0       // the last compaction of any kind, by the clock
let commanded = null      // while our own /compact runs: where its compaction is caught
let running = false       // while our own compaction runs
let beat = null
let last = null           // the last decision, for the heartbeat

async function stateDir($) {
  return policy.stateDir({
    idlepackHome: await $.env.get('IDLEPACK_HOME'),
    userProfile: await $.env.get('USERPROFILE'),
    home: await $.env.get('HOME'),
  })
}

async function readJson($, path) {
  try {
    return JSON.parse(await $.fs.read(path))
  } catch {
    return null
  }
}

async function writeJson($, path, value) {
  try {
    await $.fs.write(path, JSON.stringify(value, null, 2) + '\n')
  } catch {
    // Bookkeeping only: never worth failing a session over.
  }
}

async function loadSettings($) {
  const dir = await stateDir($)
  return policy.settings(dir ? await readJson($, `${dir}/config.json`) : null)
}

async function sessionId($) {
  try {
    const id = await $.session.id()
    return policy.isSessionId(id) ? id : null
  } catch {
    return null
  }
}

/** The cache lifetime, the last request's time and the context size, as best known now. */
async function cacheFacts($, id, turnEndAt) {
  const dir = await stateDir($)
  const probed = dir ? policy.factsFromProbe(await readJson($, `${dir}/claude/sessions/${id}.json`), id, turnEndAt) : null
  if (probed) return probed
  const read = async (fn) => {
    try {
      return await fn()
    } catch {
      return undefined
    }
  }
  const usage = await read(() => $.session.usage())
  const ttlMs = policy.inferTtl({
    force5m: await read(() => $.env.get('FORCE_PROMPT_CACHING_5M')),
    envTtl: await read(() => $.env.get('CLAUDE_CODE_PROMPT_CACHE_TTL')),
    settingTtl: await read(() => $.settings.read({ key: 'promptCacheTtl' })),
    enable1h: await read(() => $.env.get('ENABLE_PROMPT_CACHING_1H')),
    isSubscriber: Array.isArray(usage?.rateLimits) && usage.rateLimits.length > 0,
  })
  return { ttlMs, lastCallAt: null, contextTokens: usage?.context?.tokens ?? null, source: 'inferred' }
}

function disarm() {
  if (armed) {
    for (const timer of armed.timers) timer.cancel()
  }
  armed = null
  generation += 1
}

function current(token) {
  return armed !== null && armed.token === token
}

/** At a turn's end: plan once the Stop hook has spoken, then wait. */
async function arm($) {
  disarm()
  const token = generation
  const s = await loadSettings($)
  if (!policy.handles(s, headless)) return
  const id = await sessionId($)
  if (id === null) return
  const turnEndAt = await $.clock.now()
  // A turn that started while this was being read has disarmed it already.
  if (generation !== token) return
  armed = { token, id, turnEndAt, fireAt: null, ttlMs: null, source: null, timers: [] }
  armed.timers.push($.clock.after(policy.PROBE_DELAY_MS, () => void plan($, token)))
  await remember($, id, { turnEndAt })
}

async function plan($, token) {
  try {
    if (!current(token)) return
    const s = await loadSettings($)
    const facts = await cacheFacts($, armed.id, armed.turnEndAt)
    if (!current(token)) return
    if (facts.ttlMs < policy.HOUR_MS && !s.allowFiveMinuteCache) {
      last = { outcome: 'skipped', reason: '5-minute prompt cache (set allowFiveMinuteCache to include it)', at: await $.clock.now() }
      disarm()
      return
    }
    const due = policy.fireAt({
      turnEndAt: armed.turnEndAt,
      lastCallAt: facts.lastCallAt,
      ttlMs: facts.ttlMs,
      fraction: policy.fractionFor(s, headless),
    })
    armed.fireAt = due
    armed.ttlMs = facts.ttlMs
    armed.source = facts.source
    // What the threshold comes to for this model's window, for `idlepack status`; the
    // decision itself is taken again when the timer fires, with the settings then.
    try {
      const usage = await $.session.usage()
      armed.window = usage?.context?.window ?? null
      armed.threshold = policy.threshold(s, armed.window)
      armed.contextTokens = usage?.context?.tokens ?? facts.contextTokens ?? null
    } catch {
      armed.threshold = policy.threshold(s, null)
    }
    const wait = Math.max(0, due - (await $.clock.now()))
    armed.timers.push($.clock.after(wait, () => void fire($, token)))
    await remember($, armed.id, { turnEndAt: armed.turnEndAt, fireAt: due, ttlMs: facts.ttlMs })
    await heartbeat($)
  } catch {
    // A plan that could not be made leaves the session as it was.
  }
}

async function fire($, token) {
  if (!current(token) || running) return
  const plannedFor = armed
  const s = await loadSettings($)
  const now = await $.clock.now()
  const facts = await cacheFacts($, plannedFor.id, plannedFor.turnEndAt)
  let usage = null
  try {
    usage = await $.session.usage()
  } catch {
    usage = null
  }
  const tokens = usage?.context?.tokens ?? facts.contextTokens ?? undefined
  const reason = policy.reasonToSkip({
    s,
    armed: plannedFor,
    facts,
    now,
    tokens,
    windowTokens: usage?.context?.window,
    compactedAt,
    rateLimits: usage?.rateLimits,
  })
  if (!current(token)) return
  disarm()
  if (reason !== null) {
    last = { outcome: 'skipped', reason, at: now }
    await heartbeat($)
    return
  }
  running = true
  const startedGeneration = generation
  try {
    const result = await compact($, s.instructions, startedGeneration, tokens)
    const finished = await $.clock.now()
    if (result && typeof result.skip === 'string') {
      last = { outcome: 'skipped', reason: `a hook vetoed it: ${result.skip}`, at: finished }
      return
    }
    compactedAt = finished
    // What the next request will send: the window as /context estimates it, locally (no
    // request), since a compacted session has no reading of its own until it answers again.
    let after = await estimatedContext($)
    if (!Number.isFinite(after)) after = result?.tokensAfter
    const before = tokens
    last = { outcome: 'compacted', reason: '', at: finished }
    await record($, plannedFor, {
      outcome: 'compacted',
      tokens_before: before ?? null,
      tokens_after: Number.isFinite(after) ? after : null,
      duration_ms: finished - now,
      route: headless ? 'command' : 'in-process',
      usage: result?.usage ?? null,
    })
    if (s.toast) {
      const shrink = Number.isFinite(after) ? `${policy.formatTokens(before)} -> ${policy.formatTokens(after)}` : policy.formatTokens(before)
      $.ui.toast(`idlepack: compacted the idle session (${shrink}) before its prompt cache expired`)
    }
  } catch (error) {
    if (generation !== startedGeneration) {
      // A turn began while it was starting: the person came back, which is no failure.
      last = { outcome: 'skipped', reason: 'the session became active', at: await $.clock.now() }
      return
    }
    const message = error instanceof Error ? error.message : String(error)
    last = { outcome: 'failed', reason: message.slice(0, 300), at: await $.clock.now() }
    await record($, plannedFor, { outcome: 'failed', reason: last.reason, tokens_before: tokens ?? null })
  } finally {
    running = false
    await heartbeat($)
  }
}

async function estimatedContext($) {
  try {
    const usage = await $.session.usage({ breakdown: 'summary' })
    const total = usage?.context?.breakdown?.totalTokens
    return Number.isFinite(total) ? total : undefined
  } catch {
    return undefined
  }
}

/**
 * Compacts as /compact does. A terminal session lets the mod call the operation;
 * one with no terminal behind it refuses, and there /compact itself is run.
 *
 * The command's own compaction runs past this mod's hooks (Claude Code does not
 * hand a plugin the events its own call raised) and the command answers with
 * nothing to read, so the session says whether it compacted: one that has has no
 * context reading until it answers again. Measured live in Claude Code 2.1.309's
 * headless mode, the one the Desktop app's Code tab runs.
 */
async function compact($, instructions, startedAt, before) {
  if (!headless) {
    try {
      return await $.session.compact(instructions ? { instructions } : undefined)
    } catch (error) {
      // A turn that started meanwhile is a real refusal: the session is in use.
      if (generation !== startedAt) throw error
      // Otherwise Claude Code refused the mod the operation itself (no terminal
      // behind the session after all): /compact reaches the same compaction.
    }
  }
  // /compact queued behind a turn the person just started would land after it: not idle any more.
  if (generation !== startedAt) throw new Error('the session became active before it could be compacted')
  const own = { result: null }
  commanded = own
  try {
    const ran = await $.command.run({ command: 'compact', args: instructions ?? '' })
    // A build that does hand the command's compaction to this mod's hook: its own account.
    if (own.result !== null) return own.result
    let still
    try {
      still = (await $.session.usage())?.context?.tokens
    } catch {
      still = undefined
    }
    if (Number.isFinite(still) && (!Number.isFinite(before) || still >= before * 0.9)) {
      const said = typeof ran?.text === 'string' ? ran.text.trim() : ''
      throw new Error(said || '/compact did not compact the session')
    }
    return { tokensBefore: before }
  } finally {
    if (commanded === own) commanded = null
  }
}

/** What survives a reload or a restart of the session's process: when the last turn ended. */
async function remember($, id, entry) {
  try {
    await $.store.set(`s:${id}`, { ...entry, savedAt: await $.clock.now() })
  } catch {
    // The timer still runs without it.
  }
}

async function record($, plannedFor, fields) {
  const dir = await stateDir($)
  if (!dir) return
  const at = await $.clock.now()
  await writeJson($, `${dir}/events/claude-${at}-${plannedFor.id.slice(0, 8)}.json`, {
    app: 'claude',
    session_id: plannedFor.id,
    at_ms: at,
    idle_ms: at - plannedFor.turnEndAt,
    ttl_s: plannedFor.ttlMs ? Math.round(plannedFor.ttlMs / 1000) : null,
    ttl_source: plannedFor.source,
    surface,
    headless,
    mod_version: policy.VERSION,
    ...fields,
  })
}

async function heartbeat($, ended = false) {
  const dir = await stateDir($)
  const id = await sessionId($)
  if (!dir || !id) return
  // A threshold changed in the config shows within a beat, not only after the next turn.
  if (armed && armed.window !== undefined) armed.threshold = policy.threshold(await loadSettings($), armed.window)
  await writeJson($, `${dir}/claude/live/${id}.json`, {
    session_id: id,
    beat_ms: await $.clock.now(),
    ended,
    surface,
    headless,
    mod_version: policy.VERSION,
    armed: armed
      ? {
          turn_end_ms: armed.turnEndAt,
          fire_at_ms: armed.fireAt,
          ttl_s: armed.ttlMs ? armed.ttlMs / 1000 : null,
          ttl_source: armed.source,
          threshold_tokens: armed.threshold ?? null,
          window_tokens: armed.window ?? null,
          context_tokens: armed.contextTokens ?? null,
        }
      : null,
    last: last ? { outcome: last.outcome, reason: last.reason, at_ms: last.at } : null,
  })
}

/**
 * A process that started after a turn ended (a reload, the Desktop app warming a
 * session it had paused) picks the plan up again while there is time left on it.
 */
async function resume($) {
  const id = await sessionId($)
  if (!id) return
  let saved
  try {
    saved = await $.store.get(`s:${id}`)
  } catch {
    return
  }
  if (!saved || !Number.isFinite(saved.fireAt) || !Number.isFinite(saved.turnEndAt)) return
  const now = await $.clock.now()
  if (saved.fireAt <= now || (Number.isFinite(saved.compactedAt) && saved.compactedAt > saved.turnEndAt)) return
  const s = await loadSettings($)
  if (!policy.handles(s, headless)) return
  disarm()
  const token = generation
  armed = { token, id, turnEndAt: saved.turnEndAt, fireAt: saved.fireAt, ttlMs: saved.ttlMs ?? policy.HOUR_MS, source: 'resumed', timers: [] }
  armed.timers.push($.clock.after(Math.max(0, saved.fireAt - now), () => void fire($, token)))
}

/** Drops what the store holds for sessions not seen in two days. */
async function prune($) {
  try {
    const now = await $.clock.now()
    for (const key of await $.store.keys()) {
      if (!key.startsWith('s:')) continue
      const entry = await $.store.get(key)
      if (!entry || !Number.isFinite(entry.savedAt) || now - entry.savedAt > 2 * 86_400_000) await $.store.delete(key)
    }
  } catch {
    // Housekeeping only.
  }
}

export function register(on) {
  on('session.start', async ($, e, next) => {
    headless = e.isInteractive === false
    surface = e.surface ?? null
    const result = await next(e)
    await prune($)
    await resume($)
    await heartbeat($)
    if (beat) beat.cancel()
    beat = $.clock.every(policy.BEAT_MS, () => void heartbeat($))
    return result
  })

  // Any new turn on the main loop: the session is in use again.
  on('turn.start', async ($, e, next) => {
    if (e.agentId === undefined) disarm()
    return next(e)
  })

  on('turn.complete', async ($, e, next) => {
    const result = await next(e)
    if (e.agentId !== undefined || running) return result
    // Still inside the turn's own event: plan once it has returned.
    $.clock.after(0, () => void arm($))
    return result
  })

  // Any compaction of the main conversation stands the timer down (the person's /compact,
  // auto-compaction, Claude Code's own idle compaction). Claude Code 2.1.309 keeps the
  // compaction our own /compact makes away from this hook; a build that routes it here
  // hands its outcome to `compact` directly.
  on('session.compact', async ($, e, next) => {
    const result = await next(e)
    if (e.agentId !== undefined || e.trigger === 'precompute') return result
    if (result && typeof result.skip === 'string') {
      if (commanded !== null && e.trigger === 'manual') commanded.result = result
      return result
    }
    if (commanded !== null && e.trigger === 'manual') {
      commanded.result = result
      return result
    }
    compactedAt = await $.clock.now()
    if (!running) disarm()
    const id = await sessionId($)
    if (id) {
      try {
        const saved = (await $.store.get(`s:${id}`)) ?? {}
        await $.store.set(`s:${id}`, { ...saved, compactedAt, savedAt: compactedAt })
      } catch {
        // Only the restart path reads this.
      }
    }
    return result
  }).catch(($, e, next) => next(e)) // never stands in a compaction's way: what was settled stands

  on('session.end', async ($, e, next) => {
    disarm()
    if (beat) beat.cancel()
    beat = null
    await heartbeat($, true)
    return next(e)
  })
}
