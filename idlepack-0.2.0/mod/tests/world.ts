/**
 * A stand-in for everything the mod asks Claude Code for, so a test can play a
 * session's life: the session's id and context size, environment variables,
 * files under idlepack's folder, the store, the clock, compaction (in-process and
 * through /compact), settings and toasts. Each test builds one and reads back
 * what the mod did.
 */
import { mock } from 'claude-code/testing'

export const HOME = '/home/someone'
export const STATE = HOME + '/.idlepack'
export const SESSION = '3f6c2a7e-41d4-4b8e-9a51-2c0d9e1b7a10'
export const NOW = 1_790_000_000_000
export const MIN = 60_000
export const HOUR = 3_600_000
/** What a compaction leaves: the engine refuses one that leaves nothing. */
export const KEPT = [{ role: 'assistant', text: 'the summary', toolUses: [] }]

export type WorldOptions = {
  tokens?: number | null
  window?: number
  rateLimits?: { kind: string; percentUsed: number }[]
  env?: Record<string, string>
  files?: Record<string, unknown>
  settings?: Record<string, unknown>
  now?: number
  // what the engine's compaction hands back
  compacted?: { messages: unknown[]; tokensBefore?: number; tokensAfter?: number } | { skip: string }
  // how a /compact the mod runs plays out (see `engine`)
  command?: 'compacts' | 'refuses' | 'reaches-the-hook'
  // the /context estimate after a compaction
  estimate?: number
}

export function world(on: any, options: WorldOptions = {}) {
  const w = {
    files: new Map<string, string>(),
    saved: new Map<string, unknown>(),
    toasts: [] as string[],
    compactions: [] as { instructions?: string; trigger: string }[],
    commands: [] as { command: string; args: string }[],
    tokens: options.tokens === undefined ? 412_880 : options.tokens,
    clock: undefined as unknown as ReturnType<typeof mock.clock>,
    waiting: [] as { args: string; resolve: (r: unknown) => void }[],
    /**
     * Plays the engine running a queued /compact, as Claude Code 2.1.309 was seen to in
     * its headless mode: the compaction happens beneath the mod (whose own call raised it,
     * so its hooks are passed over), the context reading goes away until the next
     * response, and the command answers with nothing. `refuses` prints the engine's line
     * and compacts nothing; `reaches-the-hook` is a build that routes the compaction
     * through the plugins' session.compact hooks.
     */
    async engine($: any) {
      while (w.waiting.length > 0) {
        const { args, resolve } = w.waiting.shift()!
        const how = options.command ?? 'compacts'
        if (how === 'refuses') {
          resolve({ text: 'Not enough messages to compact.' })
        } else if (how === 'reaches-the-hook') {
          await $.session.compact({ ...(args ? { instructions: args } : {}), trigger: 'manual', messages: KEPT })
          resolve({ text: '' })
        } else {
          w.compactions.push({ instructions: args || undefined, trigger: 'manual' })
          w.tokens = null
          resolve({})
        }
      }
      await w.clock.settle()
    },
    json(path: string) {
      return JSON.parse(w.files.get(path)!)
    },
    events() {
      return [...w.files.keys()].filter((k) => k.startsWith(STATE + '/events/')).map((k) => w.json(k))
    },
  }
  for (const [path, value] of Object.entries(options.files ?? {})) w.files.set(path, JSON.stringify(value))
  const env: Record<string, string> = options.env ?? { HOME }
  on('env.get', (_$: any, e: any) => ({ value: env[e.name] }))
  on('fs.read', (_$: any, e: any) =>
    w.files.has(e.path) ? { value: w.files.get(e.path) } : { deny: 'ENOENT: ' + e.path })
  on('fs.write', (_$: any, e: any) => {
    w.files.set(e.path, e.text)
    return { value: undefined }
  })
  on('store.get', (_$: any, e: any) => ({ value: w.saved.get(e.key) }))
  on('store.set', (_$: any, e: any) => {
    w.saved.set(e.key, JSON.parse(JSON.stringify(e.value)))
    return { value: undefined }
  })
  on('store.delete', (_$: any, e: any) => {
    w.saved.delete(e.key)
    return { value: undefined }
  })
  on('store.keys', () => ({ value: [...w.saved.keys()] }))
  on('session.id', () => ({ value: SESSION }))
  on('session.usage', (_$: any, e: any) => {
    const window = options.window ?? 1_000_000
    const context: Record<string, unknown> = w.tokens === null ? { window } : { tokens: w.tokens, window }
    if (e?.breakdown) context.breakdown = { totalTokens: w.tokens ?? options.estimate ?? 48_200, categories: [] }
    return { value: { startedAt: NOW - HOUR, context, rateLimits: options.rateLimits ?? [] } }
  })
  on('settings.read', () => ({ value: options.settings ?? {} }))
  on('ui.toast', (_$: any, e: any) => {
    w.toasts.push(e.text)
    return { value: undefined }
  })
  on('session.compact', (_$: any, e: any) => {
    w.compactions.push({ instructions: e.instructions, trigger: e.trigger })
    const answer = options.compacted ?? { messages: KEPT, tokensBefore: w.tokens ?? undefined, tokensAfter: 21_300 }
    if (!('skip' in answer)) w.tokens = null // fresh window: no reading until the next response
    return answer
  })
  on('command.run', (_$: any, e: any) => {
    w.commands.push({ command: e.command, args: e.args })
    return new Promise((resolve) => w.waiting.push({ args: e.args, resolve }))
  })
  on('session.start', (_$: any, e: any) => ({ cwd: e.cwd }))
  on('session.end', () => ({ sessionId: SESSION }))
  on('turn.start', (_$: any, e: any) => ({ turnId: e.turnId }))
  on('turn.complete', () => ({ text: '' }))
  w.clock = mock.clock(on, { now: options.now ?? NOW })
  return w
}

/** The Stop hook's record for the turn that ended at `at`: a 1h cache, unless told otherwise. */
export function probe(at: number, fields: Record<string, unknown> = {}) {
  return {
    [STATE + '/claude/sessions/' + SESSION + '.json']: {
      session_id: SESSION,
      last_call_ms: at - 2_000,
      ttl_s: 3600,
      context_tokens: 412_880,
      written_ms: at + 1_000,
      ...fields,
    },
  }
}

export const DESKTOP = { cwd: '/work/project', surface: 'desktop', isInteractive: false } as const
export const TERMINAL = { cwd: '/work/project', surface: 'terminal', isInteractive: true } as const

export function turnEnd(fields: Record<string, unknown> = {}) {
  return { turnId: 't1', answer: 'done', durationMs: 1_000, isAborted: false, reason: 'answer', ...fields }
}
