import { fileURLToPath } from 'node:url'
import path from 'node:path'

/** Where a suite's tests run. */
export type EnvironmentKind = 'soroban-sim' | 'local'

export interface SimConfig {
  /** The `soroban-sim` binary. */
  bin: string
  /** The snapshot the setup run's server starts from. */
  snapshot: string
  /** `off` runs setup every time and leaves the cache alone. */
  cache: 'on' | 'off'
  cacheDir: string
  /** `on` says on stderr, for each suite, whether its setup came from the cache and, on a miss, what changed. */
  explain: boolean
}

export interface LocalConfig {
  rpcUrl: string
  friendbotUrl: string
}

export interface Config {
  environment: EnvironmentKind
  /** How long to wait for a sent transaction's result, in seconds; separate from its time bound. */
  waitSeconds: number
  sim: SimConfig
  local: LocalConfig
}

/** The settings and latest header of a fresh quickstart local network, so the default base shares its passphrase. */
export const DEFAULT_SNAPSHOT = fileURLToPath(new URL('../snapshots/local-network.json', import.meta.url))

/** Mixed into every signer's key derivation. */
export const SEED = 'soroban-sim-vitest'

/** The harness's own wait timer runs this much past `waitSeconds`, so the SDK's poll, bounded the same, reports first. */
export const WAIT_MARGIN_MS = 2_000

/** The longest wait a Node timer can hold (2^31 - 1 ms), less the margin. */
export const MAX_WAIT_SECONDS = Math.floor((2 ** 31 - 1 - WAIT_MARGIN_MS) / 1000)

export function defaultCacheDir(cwd = process.cwd()): string {
  return path.join(cwd, 'node_modules', '.cache', 'soroban-sim-vitest')
}

type Env = Record<string, string | undefined>

/** Reads the configuration from environment variables (see the README's table). */
export function resolveConfig(env: Env = process.env): Config {
  const environment = (env.SOROBAN_TEST_ENV || 'soroban-sim') as EnvironmentKind
  if (environment !== 'soroban-sim' && environment !== 'local') {
    throw new Error(`SOROBAN_TEST_ENV must be "soroban-sim" or "local", not ${JSON.stringify(environment)}`)
  }
  const cache = (env.SOROBAN_TEST_CACHE || 'on') as SimConfig['cache']
  if (cache !== 'on' && cache !== 'off') {
    throw new Error(`SOROBAN_TEST_CACHE must be "on" or "off", not ${JSON.stringify(cache)}`)
  }
  const explain = env.SOROBAN_TEST_CACHE_EXPLAIN || 'off'
  if (explain !== 'on' && explain !== 'off') {
    throw new Error(`SOROBAN_TEST_CACHE_EXPLAIN must be "on" or "off", not ${JSON.stringify(explain)}`)
  }
  const rpcUrl = env.SOROBAN_TEST_RPC_URL || 'http://localhost:8000/rpc'
  const waitSeconds = Number(env.SOROBAN_TEST_WAIT_SECONDS || 60)
  if (!(waitSeconds > 0 && waitSeconds <= MAX_WAIT_SECONDS)) {
    throw new Error(
      `SOROBAN_TEST_WAIT_SECONDS must be a number of seconds above 0 and at most ${MAX_WAIT_SECONDS} ` +
        `(the longest a timer can run), not ${JSON.stringify(env.SOROBAN_TEST_WAIT_SECONDS)}`,
    )
  }
  return {
    environment,
    waitSeconds,
    sim: {
      bin: env.SOROBAN_SIM_BIN || 'soroban-sim',
      snapshot: path.resolve(env.SOROBAN_SIM_SNAPSHOT || DEFAULT_SNAPSHOT),
      cache,
      cacheDir: path.resolve(env.SOROBAN_TEST_CACHE_DIR || defaultCacheDir()),
      explain: explain === 'on',
    },
    local: {
      rpcUrl,
      friendbotUrl: env.SOROBAN_TEST_FRIENDBOT_URL || new URL('/friendbot', rpcUrl).href,
    },
  }
}
