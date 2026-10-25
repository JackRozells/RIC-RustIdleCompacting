import { expect, test } from 'claude-code/testing'
import { DESKTOP, HOUR, KEPT, MIN, NOW, SESSION, STATE, TERMINAL, probe, turnEnd, world } from './world.ts'
import * as policy from '../hooks/policy.js'

// The Stop hook's facts say the last request was 2 s before the turn ended; 90% of an hour on.
const DUE = NOW - 2_000 + 0.9 * HOUR

test('a Desktop session is compacted through /compact at 90% of its one-hour cache', async ($, on) => {
  const w = world(on, { files: probe(NOW) })
  await $.session.start(DESKTOP)
  await $.turn.complete(turnEnd())
  await w.clock.settle()
  await w.clock.set(DUE - 1)
  expect(w.commands).toEqual([])
  await w.clock.set(DUE)
  expect(w.commands).toEqual([{ command: 'compact', args: '' }])
  await w.engine($)
  expect(w.compactions).toEqual([{ instructions: undefined, trigger: 'manual' }])
  const [event] = w.events()
  expect(event.outcome).toBe('compacted')
  expect(event.session_id).toBe(SESSION)
  expect(event.tokens_before).toBe(412_880)
  // After: the /context estimate, read locally, since a compacted session has no reading.
  expect(event.tokens_after).toBe(48_200)
  expect(event.route).toBe('command')
  expect(event.ttl_s).toBe(3600)
  expect(event.ttl_source).toBe('stop-hook')
  expect(event.headless).toBe(true)
  expect(w.toasts.length).toBe(1)
  expect(w.toasts[0]).toContain('413k -> 48k')
  const live = w.json(STATE + '/claude/live/' + SESSION + '.json')
  expect(live.last.outcome).toBe('compacted')
  expect(live.armed).toBe(null)
})

test('a terminal session is compacted in-process, a little after Claude Code would', async ($, on) => {
  const w = world(on, { files: probe(NOW) })
  await $.session.start(TERMINAL)
  await $.turn.complete(turnEnd())
  await w.clock.settle()
  await w.clock.set(DUE)
  expect(w.compactions).toEqual([])
  await w.clock.set(NOW - 2_000 + 0.92 * HOUR)
  // In-process: the operation itself, no /compact command run.
  expect(w.commands).toEqual([])
  expect(w.compactions.length).toBe(1)
  expect(w.events()[0].outcome).toBe('compacted')
  expect(w.events()[0].headless).toBe(false)
})

test("Claude Code's own idle compaction gets there first: the mod stands down", async ($, on) => {
  const w = world(on, { files: probe(NOW) })
  await $.session.start(TERMINAL)
  await $.turn.complete(turnEnd())
  await w.clock.settle()
  await w.clock.set(DUE)
  await $.session.compact({ trigger: 'auto', messages: KEPT })
  await w.clock.set(NOW + HOUR)
  expect(w.compactions.length).toBe(1)
  expect(w.events()).toEqual([])
})

test('a new turn before the timer fires disarms it', async ($, on) => {
  const w = world(on, { files: probe(NOW) })
  await $.session.start(DESKTOP)
  await $.turn.complete(turnEnd())
  await w.clock.settle()
  await w.clock.advance(30 * MIN)
  await $.turn.start({ text: 'back again', turnId: 't2' })
  await w.clock.set(NOW + 2 * HOUR)
  expect(w.commands).toEqual([])
  expect(w.events()).toEqual([])
})

test("a subagent's turn neither arms nor disarms", async ($, on) => {
  const w = world(on, { files: probe(NOW) })
  await $.session.start(DESKTOP)
  await $.turn.complete(turnEnd())
  await w.clock.settle()
  await $.turn.start({ text: '', turnId: 'a1', agentId: 'agent-1' })
  await $.turn.complete(turnEnd({ turnId: 'a1', agentId: 'agent-1' }))
  await w.clock.set(DUE)
  expect(w.commands.length).toBe(1)
})

test('a small session is left alone', async ($, on) => {
  const w = world(on, { tokens: 120_000, files: probe(NOW, { context_tokens: 120_000 }) })
  await $.session.start(DESKTOP)
  await $.turn.complete(turnEnd())
  await w.clock.settle()
  await w.clock.set(DUE)
  expect(w.commands).toEqual([])
  const live = w.json(STATE + '/claude/live/' + SESSION + '.json')
  expect(live.last.outcome).toBe('skipped')
  expect(live.last.reason).toContain('small')
})

test('auto threshold: half of a 200k window, so a 120k session there is compacted', async ($, on) => {
  const w = world(on, { tokens: 120_000, window: 200_000, files: probe(NOW, { context_tokens: 120_000 }) })
  await $.session.start(DESKTOP)
  await $.turn.complete(turnEnd())
  await w.clock.settle()
  await w.clock.set(DUE)
  expect(w.commands.length).toBe(1)
})

test('a threshold written as "150k" leaves a 120k session alone and compacts a 160k one', async ($, on) => {
  const config = { [STATE + '/config.json']: { claude: { minTokens: '150k' } } }
  const small = world(on, { tokens: 120_000, files: { ...probe(NOW, { context_tokens: 120_000 }), ...config } })
  await $.session.start(DESKTOP)
  await $.turn.complete(turnEnd())
  await small.clock.settle()
  await small.clock.advance(30_000)
  // The heartbeat already says why, long before the timer would fire.
  const live = small.json(STATE + '/claude/live/' + SESSION + '.json')
  expect(live.armed.threshold_tokens).toBe(150_000)
  expect(live.armed.context_tokens).toBe(120_000)
  await small.clock.set(DUE)
  expect(small.commands).toEqual([])
  expect(small.json(STATE + '/claude/live/' + SESSION + '.json').last.reason).toContain('under the 150k threshold')
})

test('a share of the window: 40% of a 200k window is 80k, so a 100k session is compacted', async ($, on) => {
  const w = world(on, {
    tokens: 100_000,
    window: 200_000,
    files: { ...probe(NOW, { context_tokens: 100_000 }), [STATE + '/config.json']: { claude: { minTokens: '40%' } } },
  })
  await $.session.start(DESKTOP)
  await $.turn.complete(turnEnd())
  await w.clock.settle()
  await w.clock.set(DUE)
  expect(w.commands.length).toBe(1)
})

test('a configured threshold is used as given', async ($, on) => {
  const w = world(on, {
    tokens: 300_000,
    files: { ...probe(NOW), [STATE + '/config.json']: { claude: { minTokens: 350_000 } } },
  })
  await $.session.start(DESKTOP)
  await $.turn.complete(turnEnd())
  await w.clock.settle()
  await w.clock.set(DUE)
  expect(w.commands).toEqual([])
})

test('a 5-minute cache is not compacted by default', async ($, on) => {
  const w = world(on, { files: probe(NOW, { ttl_s: 300 }) })
  await $.session.start(DESKTOP)
  await $.turn.complete(turnEnd())
  await w.clock.settle()
  await w.clock.set(NOW + 2 * HOUR)
  expect(w.commands).toEqual([])
  expect(w.json(STATE + '/claude/live/' + SESSION + '.json').last.reason).toContain('5-minute')
})

test('a 5-minute cache is compacted at 4.5 minutes when allowed', async ($, on) => {
  const w = world(on, {
    files: { ...probe(NOW, { ttl_s: 300 }), [STATE + '/config.json']: { claude: { allowFiveMinuteCache: true } } },
  })
  await $.session.start(DESKTOP)
  await $.turn.complete(turnEnd())
  await w.clock.settle()
  await w.clock.set(NOW - 2_000 + 0.9 * 300_000)
  expect(w.commands.length).toBe(1)
})

test('without the Stop hook a subscriber is taken to have the one-hour cache', async ($, on) => {
  const w = world(on, { rateLimits: [{ kind: 'five_hour', percentUsed: 20 }] })
  await $.session.start(DESKTOP)
  await $.turn.complete(turnEnd())
  await w.clock.settle()
  await w.clock.set(NOW + 0.9 * HOUR)
  expect(w.commands.length).toBe(1)
  await w.engine($)
  expect(w.events()[0].ttl_source).toBe('inferred')
})

test('without the Stop hook an API-key session (no rate-limit windows) is taken as 5-minute', async ($, on) => {
  const w = world(on)
  await $.session.start(DESKTOP)
  await $.turn.complete(turnEnd())
  await w.clock.settle()
  await w.clock.set(NOW + 2 * HOUR)
  expect(w.commands).toEqual([])
})

test('the promptCacheTtl setting outranks the subscriber default', async ($, on) => {
  const w = world(on, { rateLimits: [{ kind: 'five_hour', percentUsed: 1 }], settings: { promptCacheTtl: '5m' } })
  await $.session.start(DESKTOP)
  await $.turn.complete(turnEnd())
  await w.clock.settle()
  await w.clock.set(NOW + 2 * HOUR)
  expect(w.commands).toEqual([])
})

test('a timer that fires late (the computer slept) has lapsed', async () => {
  // The mocked clock runs every timer on time, so the rule itself is read here.
  const armed = { turnEndAt: NOW, fireAt: DUE, ttlMs: HOUR }
  const facts = { ttlMs: HOUR, lastCallAt: NOW - 2_000, contextTokens: 412_880 }
  const s = policy.settings({})
  const ask = (now: number) =>
    policy.reasonToSkip({ s, armed, facts, now, tokens: 412_880, windowTokens: 1_000_000, compactedAt: 0, rateLimits: [] })
  expect(ask(DUE)).toBe(null)
  expect(ask(DUE + 59_000)).toBe(null)
  expect(ask(DUE + 2 * MIN)).toContain('lapsed')
})

test('the expiry margin scales with a short cache', async () => {
  const s = policy.settings({ claude: { allowFiveMinuteCache: true } })
  const turnEndAt = NOW
  const due = policy.fireAt({ turnEndAt, lastCallAt: NOW - 2_000, ttlMs: 300_000, fraction: 0.9 })
  const armed = { turnEndAt, fireAt: due, ttlMs: 300_000 }
  const facts = { ttlMs: 300_000, lastCallAt: NOW - 2_000, contextTokens: 412_880 }
  expect(policy.reasonToSkip({ s, armed, facts, now: due, tokens: 412_880, windowTokens: 1_000_000, compactedAt: 0, rateLimits: [] })).toBe(null)
  expect(policy.reasonToSkip({ s, armed, facts, now: NOW + 290_000, tokens: 412_880, windowTokens: 1_000_000, compactedAt: 0, rateLimits: [] })).toContain('expired')
})

test('settings are checked: out-of-range fractions are clamped, a tiny threshold is raised to the floor', async () => {
  const s = policy.settings({ claude: { fireAtFraction: 2, terminalFireAtFraction: 0.1, minTokens: 10, terminal: 'sometimes' } })
  expect(s.fireAtFraction).toBe(0.95)
  expect(s.terminalFireAtFraction).toBe(0.5)
  expect(policy.threshold(s, 1_000_000)).toBe(policy.FLOOR_TOKENS)
  expect(s.terminal).toBe('auto')
  expect(policy.threshold(policy.settings({}), 1_000_000)).toBe(200_000)
  expect(policy.threshold(policy.settings({}), 200_000)).toBe(100_000)
  expect(policy.threshold(policy.settings({}), 64_000)).toBe(32_000)
  expect(policy.threshold(policy.settings({}), 30_000)).toBe(policy.FLOOR_TOKENS)
})

test('thresholds read as the idlepack command writes them, and as people type them', async () => {
  const at = (minTokens: unknown, window: number) => policy.threshold(policy.settings({ claude: { minTokens } }), window)
  expect(at(150_000, 1_000_000)).toBe(150_000)
  expect(at('150k', 1_000_000)).toBe(150_000)
  expect(at('150,000', 1_000_000)).toBe(150_000)
  expect(at('1.2m', 1_000_000)).toBe(1_200_000)
  expect(at('40%', 1_000_000)).toBe(400_000)
  expect(at('40%', 200_000)).toBe(80_000)
  expect(at('auto', 200_000)).toBe(100_000)
  // Anything unreadable is auto, never "compact everything".
  for (const bad of ['lots', '-5k', '0', '0%', '150kb', '1e3k', true, null]) expect(at(bad, 1_000_000)).toBe(200_000)
})

test('the home folder: a Windows profile, else HOME, else the override', async () => {
  expect(policy.stateDir({ userProfile: 'C:\\Users\\jack', home: '/c/Users/jack' })).toBe('C:/Users/jack/.idlepack')
  expect(policy.stateDir({ home: '/Users/jack/' })).toBe('/Users/jack/.idlepack')
  expect(policy.stateDir({ idlepackHome: '/tmp/ip', home: '/Users/jack' })).toBe('/tmp/ip')
  expect(policy.stateDir({})).toBe(null)
})

test('a nearly full rate-limit window is left alone', async ($, on) => {
  const w = world(on, { files: probe(NOW), rateLimits: [{ kind: 'five_hour', percentUsed: 97 }] })
  await $.session.start(DESKTOP)
  await $.turn.complete(turnEnd())
  await w.clock.settle()
  await w.clock.set(DUE)
  expect(w.commands).toEqual([])
})

test('switched off in config, nothing happens', async ($, on) => {
  const w = world(on, { files: { ...probe(NOW), [STATE + '/config.json']: { claude: { enabled: false } } } })
  await $.session.start(DESKTOP)
  await $.turn.complete(turnEnd())
  await w.clock.settle()
  await w.clock.set(NOW + 2 * HOUR)
  expect(w.commands).toEqual([])
  expect(w.compactions).toEqual([])
})

test('terminal "off" leaves terminal sessions to Claude Code', async ($, on) => {
  const w = world(on, { files: { ...probe(NOW), [STATE + '/config.json']: { claude: { terminal: 'off' } } } })
  await $.session.start(TERMINAL)
  await $.turn.complete(turnEnd())
  await w.clock.settle()
  await w.clock.set(NOW + 2 * HOUR)
  expect(w.compactions).toEqual([])
})

test('instructions from config ride after /compact', async ($, on) => {
  const w = world(on, {
    files: { ...probe(NOW), [STATE + '/config.json']: { claude: { instructions: 'keep the open TODOs' } } },
  })
  await $.session.start(DESKTOP)
  await $.turn.complete(turnEnd())
  await w.clock.settle()
  await w.clock.set(DUE)
  expect(w.commands).toEqual([{ command: 'compact', args: 'keep the open TODOs' }])
  await w.engine($)
  expect(w.compactions[0].instructions).toBe('keep the open TODOs')
})

test('a /compact that compacts nothing is a failure, in the engine\'s words', async ($, on) => {
  const w = world(on, { files: probe(NOW), command: 'refuses' })
  await $.session.start(DESKTOP)
  await $.turn.complete(turnEnd())
  await w.clock.settle()
  await w.clock.set(DUE)
  await w.engine($)
  const [event] = w.events()
  expect(event.outcome).toBe('failed')
  expect(event.reason).toBe('Not enough messages to compact.')
  expect(w.toasts).toEqual([])
})

test('a build that routes the command\'s compaction through the hook is read from it', async ($, on) => {
  const w = world(on, { files: probe(NOW), command: 'reaches-the-hook' })
  await $.session.start(DESKTOP)
  await $.turn.complete(turnEnd())
  await w.clock.settle()
  await w.clock.set(DUE)
  await w.engine($)
  expect(w.events()[0].outcome).toBe('compacted')
})

test('a hook that vetoes the compaction is a skip, not a failure', async ($, on) => {
  const w = world(on, { files: probe(NOW), compacted: { skip: 'not now' }, command: 'reaches-the-hook' })
  await $.session.start(DESKTOP)
  await $.turn.complete(turnEnd())
  await w.clock.settle()
  await w.clock.set(DUE)
  await w.engine($)
  expect(w.events()).toEqual([])
  expect(w.json(STATE + '/claude/live/' + SESSION + '.json').last.outcome).toBe('skipped')
})

test('a stale Stop-hook record (an earlier turn) falls back to inference', async ($, on) => {
  const w = world(on, { files: probe(NOW - 2 * HOUR, { ttl_s: 300 }), rateLimits: [{ kind: 'five_hour', percentUsed: 3 }] })
  await $.session.start(DESKTOP)
  await $.turn.complete(turnEnd())
  await w.clock.settle()
  await w.clock.set(NOW + 0.9 * HOUR)
  expect(w.commands.length).toBe(1)
})

test('a restarted session picks the plan up from the store while time is left', async ($, on) => {
  const w = world(on, { files: probe(NOW) })
  w.saved.set('s:' + SESSION, { turnEndAt: NOW, fireAt: DUE, ttlMs: HOUR, savedAt: NOW })
  await w.clock.set(NOW + 20 * MIN)
  await $.session.start(DESKTOP)
  await w.clock.set(DUE)
  expect(w.commands.length).toBe(1)
})

test('a restarted session whose plan was already compacted does nothing', async ($, on) => {
  const w = world(on, { files: probe(NOW) })
  w.saved.set('s:' + SESSION, { turnEndAt: NOW, fireAt: DUE, ttlMs: HOUR, savedAt: NOW, compactedAt: NOW + MIN })
  await w.clock.set(NOW + 20 * MIN)
  await $.session.start(DESKTOP)
  await w.clock.set(DUE + MIN)
  expect(w.commands).toEqual([])
})

test('the heartbeat shows the armed timer', async ($, on) => {
  const w = world(on, { files: probe(NOW) })
  await $.session.start(DESKTOP)
  await $.turn.complete(turnEnd())
  await w.clock.settle()
  await w.clock.advance(30_000)
  const live = w.json(STATE + '/claude/live/' + SESSION + '.json')
  expect(live.armed.fire_at_ms).toBe(DUE)
  expect(live.armed.ttl_s).toBe(3600)
  expect(live.headless).toBe(true)
  expect(live.surface).toBe('desktop')
})

test('session end writes a final heartbeat and cancels the timer', async ($, on) => {
  const w = world(on, { files: probe(NOW) })
  await $.session.start(DESKTOP)
  await $.turn.complete(turnEnd())
  await w.clock.settle()
  await $.session.end({ reason: 'other', sessionId: SESSION })
  await w.clock.set(NOW + 2 * HOUR)
  expect(w.commands).toEqual([])
  expect(w.json(STATE + '/claude/live/' + SESSION + '.json').ended).toBe(true)
})
