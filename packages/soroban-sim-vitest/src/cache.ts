import { createHash, randomBytes } from 'node:crypto'
import { execFile } from 'node:child_process'
import { promisify } from 'node:util'
import { readFile, rename, rm, mkdir, writeFile, access } from 'node:fs/promises'
import { delimiter, dirname, join } from 'node:path'
import { SEED, type Config } from './config.ts'
import type { AnySuite } from './environments.ts'
import { toPath } from './paths.ts'
import { encode } from './setup-values.ts'
import { FUNDING_XLM } from './worker.ts'

const KEY_FORMAT = 'soroban-sim-vitest/cache/3'

/** What setup produced besides the snapshot: its result as JSON, and the accounts it created. */
export interface StoredSetup {
  result: unknown
  accounts: string[]
}

export interface CacheEntry extends StoredSetup {
  snapshot: string
}

const sha256 = (data: string | Uint8Array) => createHash('sha256').update(data).digest('hex')

/** What decides a suite's setup snapshot, each part hashed: the cache key is a hash of all of them. */
export interface KeyParts {
  format: string
  suite: string
  setup: string
  testFile: string | null
  seed: string
  funding: string
  wasm: string[]
  files: string[]
  inputs: Record<string, string>
  binary: string
  base: string
}

/**
 * The key of a suite's setup snapshot: a hash of everything that decides it.
 * `testFile` is the file that defines the suite, for the values and helpers
 * setup takes from it.
 */
export async function cacheKey(
  def: AnySuite,
  config: Config,
  wasm: readonly Buffer[],
  testFile?: string,
): Promise<{ key: string; parts: KeyParts }> {
  const { snapshot, bin } = config.sim
  const inputs: Record<string, string> = {}
  for (const [name, value] of Object.entries(def.cacheInputs ?? {})) {
    inputs[name] = sha256(JSON.stringify(encode(value, `cacheInputs.${name}`) ?? null))
  }
  const parts: KeyParts = {
    format: KEY_FORMAT,
    suite: def.name,
    setup: sha256(def.setup.toString()),
    testFile: testFile === undefined ? null : sha256(await readFile(testFile)),
    seed: SEED,
    funding: FUNDING_XLM,
    wasm: wasm.map((code) => sha256(code)).sort(),
    files: await Promise.all((def.cacheFiles ?? []).map(async (f) => sha256(await readFile(toPath(f))))),
    inputs,
    binary: sha256(JSON.stringify(await binaryIdentity(bin))),
    base: sha256(await readFile(snapshot)),
  }
  return { key: sha256(JSON.stringify(parts)).slice(0, 32), parts }
}

const LABELS: Record<Exclude<keyof KeyParts, 'inputs'>, string> = {
  format: 'the cache format',
  suite: "the suite's name",
  setup: "setup's source",
  testFile: 'the test file',
  seed: 'the account seed',
  funding: 'the account funding',
  wasm: 'the Wasm',
  files: 'cacheFiles',
  binary: 'the soroban-sim binary',
  base: 'the base snapshot',
}

const explainRecord = (dir: string, def: AnySuite, testFile: string | undefined) =>
  join(dir, '.explain', `${sha256(`${def.name}\0${testFile ?? ''}`).slice(0, 32)}.json`)

/**
 * Notes `key` and its parts as the last entry this suite (by name and test
 * file) used: call it once the entry is there, after a hit or a stored setup.
 */
export async function recordEntry(dir: string, def: AnySuite, testFile: string | undefined, key: string, parts: KeyParts): Promise<void> {
  const record = explainRecord(dir, def, testFile)
  await mkdir(dirname(record), { recursive: true })
  await writeFile(record, JSON.stringify({ key, parts }))
}

/**
 * Says whether `key` was a hit and, on a miss, which parts changed since the
 * last entry this suite (by name and test file) used.
 */
export async function explainCache(
  dir: string,
  def: AnySuite,
  testFile: string | undefined,
  key: string,
  parts: KeyParts,
  hit: boolean,
): Promise<string> {
  const record = explainRecord(dir, def, testFile)
  const suite = JSON.stringify(def.name)
  let previous: { key: string; parts: KeyParts } | undefined
  try {
    previous = JSON.parse(await readFile(record, 'utf8')) as { key: string; parts: KeyParts }
  } catch {
    previous = undefined
  }
  if (hit) return `soroban-sim-vitest: cache hit for ${suite} (${key}): setup did not run`
  if (!previous) return `soroban-sim-vitest: cache miss for ${suite} (${key}): no earlier entry for this suite`
  const before = previous.parts
  const changed = (Object.keys(LABELS) as (keyof typeof LABELS)[])
    .filter((part) => JSON.stringify(before[part]) !== JSON.stringify(parts[part]))
    .map((part) => LABELS[part])
  const names = new Set([...Object.keys(before.inputs ?? {}), ...Object.keys(parts.inputs)])
  for (const name of [...names].sort()) {
    if (before.inputs?.[name] !== parts.inputs[name]) changed.push(`cacheInputs.${name}`)
  }
  if (changed.length === 0) return `soroban-sim-vitest: cache miss for ${suite} (${key}): its entry is gone (the cache was cleared?)`
  return `soroban-sim-vitest: cache miss for ${suite} (${key}): changed since the last entry: ${changed.join(', ')}`
}

const identities = new Map<string, Promise<{ version: string; sha256: string }>>()

/** `soroban-sim --version` and a hash of the binary itself, since a local build keeps its version. */
export function binaryIdentity(bin: string): Promise<{ version: string; sha256: string }> {
  let found = identities.get(bin)
  if (!found) {
    found = (async () => {
      const file = await which(bin)
      const [{ stdout }, bytes] = await Promise.all([promisify(execFile)(file, ['--version']), readFile(file)])
      return { version: stdout.trim(), sha256: sha256(bytes) }
    })()
    identities.set(bin, found)
  }
  return found
}

async function which(bin: string): Promise<string> {
  if (bin.includes('/') || bin.includes('\\')) return bin
  for (const dir of (process.env.PATH ?? '').split(delimiter)) {
    const candidate = join(dir, bin)
    try {
      await access(candidate)
      return candidate
    } catch {
      // not in this directory
    }
  }
  throw new Error(`${bin} is not on PATH; build it (cargo build --release -p soroban-sim-cli) and set SOROBAN_SIM_BIN to it`)
}

export async function readEntry(dir: string, key: string): Promise<CacheEntry | undefined> {
  const entry = join(dir, key)
  try {
    const setup = JSON.parse(await readFile(join(entry, 'setup.json'), 'utf8')) as StoredSetup
    if (!Array.isArray(setup.accounts)) return undefined
    await access(join(entry, 'snapshot.json'))
    return { snapshot: join(entry, 'snapshot.json'), result: setup.result, accounts: setup.accounts }
  } catch {
    return undefined
  }
}

const occupied = (e: unknown) => ['EEXIST', 'ENOTEMPTY'].includes((e as NodeJS.ErrnoException).code ?? '')

/**
 * Stores a setup's snapshot, result and accounts under `key` and returns
 * the entry now there, as read back from disk: this worker's, or the one
 * another worker stored first (all of it). The entry is written aside and
 * renamed into place, so a reader sees all of it or none.
 */
export async function writeEntry(dir: string, key: string, snapshot: string, stored: StoredSetup): Promise<CacheEntry> {
  const staging = join(dir, `.staging-${randomBytes(6).toString('hex')}`)
  await mkdir(staging, { recursive: true })
  await writeFile(join(staging, 'snapshot.json'), snapshot)
  await writeFile(join(staging, 'setup.json'), JSON.stringify({ result: stored.result, accounts: stored.accounts }))
  const entry = join(dir, key)
  try {
    await rename(staging, entry).catch((e) => {
      if (!occupied(e)) throw e
    })
    const installed = await readEntry(dir, key)
    if (!installed) {
      throw new Error(
        `the cached setup ${entry} cannot be read, and is not replaced: ` +
          `clear the cache (delete ${dir}, or call clearCache) and run again`,
      )
    }
    return installed
  } finally {
    await rm(staging, { recursive: true, force: true })
  }
}

/** Deletes every cached setup snapshot under `dir`. */
export async function clearCache(dir: string): Promise<void> {
  await rm(dir, { recursive: true, force: true })
}
