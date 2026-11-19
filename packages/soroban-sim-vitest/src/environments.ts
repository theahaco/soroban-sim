import { createHash } from 'node:crypto'
import { mkdtempSync, rmSync } from 'node:fs'
import { readFile, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { cacheKey, explainCache, readEntry, recordEntry, writeEntry, type StoredSetup } from './cache.ts'
import { SEED, type Config } from './config.ts'
import { attach } from './account.ts'
import { isDefinition, type ContractDefinition } from './clients.ts'
import { ACCOUNTS, ADMIN, ADOPT, BINDINGS, DEPLOYS } from './internal.ts'
import { RUN_NONCE } from './keys.ts'
import { toPath } from './paths.ts'
import { decode, encode, type Stored } from './setup-values.ts'
import { Worker } from './worker.ts'

/** A Wasm file, or a contract declared with `defineContract` (its Wasm and, optionally, its generated client). */
export type WasmEntry = string | URL | ContractDefinition<any>

export interface SuiteDefinition<S> {
  /** Names the suite in messages and cache entries. */
  name: string
  /**
   * The Wasm files setup deploys: their contents are part of the cache key,
   * and setup may deploy no others. A file paired with its generated client
   * makes every client for that code, in setup and in the tests, of that class.
   */
  wasm?: readonly WasmEntry[]
  /**
   * Files besides the Wasm and the test file whose contents setup depends
   * on (helper modules it imports, data it reads): the cache key covers the
   * test file only, so list them here for a change to them to rerun it.
   */
  cacheFiles?: readonly (string | URL)[]
  /**
   * Values setup depends on that no file holds: environment variables it
   * reads, arguments of a factory that builds the suite. Each is part of the
   * cache key, by name, so a change to one reruns setup. Stored as setup's
   * result is (JSON values, bigints, bytes).
   */
  cacheInputs?: Record<string, unknown>
  /**
   * Creates accounts, deploys, writes; returns what the tests need: JSON
   * values, bigints, bytes, accounts and contract clients. Each test gets
   * its own copy, with the accounts and clients on its own network; any
   * other value is refused, naming where in the result it is.
   */
  setup: (worker: Worker) => Promise<S>
}

export type AnySuite = SuiteDefinition<unknown>

/** A test's worker and setup, and how to give them back. */
export interface Lease<S> {
  worker: Worker
  setup: S
  release(): Promise<void>
}

export interface Environment<S> {
  /** `testFile` is the file the test is in, which defines the suite. */
  acquire(testId: string, testFile: string): Promise<Lease<S>>
}

export function environmentFor<S>(def: SuiteDefinition<S>, config: Config): Environment<S> {
  return config.environment === 'local' ? localEnvironment(def, config) : simEnvironment(def, config)
}

const fileOf = (entry: WasmEntry) => (isDefinition(entry) ? entry.file : entry)

async function loadWasm(def: AnySuite): Promise<Buffer[]> {
  return Promise.all((def.wasm ?? []).map((entry) => readFile(toPath(fileOf(entry)))))
}

const sha256 = (code: Buffer) => createHash('sha256').update(code).digest('hex')
const hashes = (wasm: Buffer[]) => new Set(wasm.map(sha256))

/** Tells `worker` which generated client each of the suite's Wasm files has. */
function pairBindings(def: AnySuite, worker: Worker, wasm: Buffer[]): void {
  ;(def.wasm ?? []).forEach((entry, i) => {
    if (isDefinition(entry) && entry.client) worker[BINDINGS].set(sha256(wasm[i]), entry.client)
  })
}

/** Runs setup on `worker` and returns its result as stored: JSON, with markers for accounts and contracts. */
async function runSetup(def: AnySuite, worker: Worker, wasm: Buffer[]): Promise<StoredSetup> {
  worker[DEPLOYS] = hashes(wasm)
  try {
    const value = await def.setup(worker)
    let result: Stored | undefined
    try {
      result = encode(value)
    } catch (e) {
      throw new Error(`${def.name}: setup's ${(e as Error).message}`)
    }
    return { result, accounts: worker[ACCOUNTS]() }
  } finally {
    worker[DEPLOYS] = undefined
  }
}

/** A stored setup result with its accounts and contract clients bound to `worker`, each client with the same signer. */
async function revive<S>(stored: StoredSetup, worker: Worker): Promise<S> {
  worker[ADOPT](stored.accounts)
  return (await decode(stored.result as Stored | undefined, (marker) => {
    if (marker.name !== undefined && marker.id === undefined) return worker.account(marker.name as string)
    const signer = marker.signer === null ? undefined : worker.account(marker.signer as string)
    return attach(worker, marker.id as string, undefined, signer, marker.wasmHash as string | null)
  })) as S
}

// --- soroban-sim ---

export interface PreparedSetup extends StoredSetup {
  /** The snapshot each test's server starts from. */
  snapshot: string
  /** True when the snapshot came from the cache and setup did not run. */
  cached: boolean
  /** With the cache on: whether it was a hit, and on a miss which inputs changed since this suite's last entry. */
  explanation?: string
}

let scratch: string | undefined

function scratchDir(): string {
  if (!scratch) {
    const dir = mkdtempSync(join(tmpdir(), 'soroban-sim-vitest-'))
    process.once('exit', () => rmSync(dir, { recursive: true, force: true }))
    scratch = dir
  }
  return scratch
}

/**
 * Runs the suite's setup once on a fresh `soroban-sim serve` and exports the
 * result with `fork_exportSnapshot`, or takes that snapshot from the cache.
 * `testFile` is the file that defines the suite: its contents are part of
 * the cache key.
 */
export async function prepareSimSetup(def: AnySuite, config: Config, testFile?: string): Promise<PreparedSetup> {
  const wasm = await loadWasm(def)
  const { key, parts } = await cacheKey(def, config, wasm, testFile)
  const { cache, cacheDir, explain } = config.sim
  if (cache === 'on') {
    const hit = await readEntry(cacheDir, key)
    const explanation = await explainCache(cacheDir, def, testFile, key, parts, hit !== undefined)
    if (explain) process.stderr.write(`${explanation}\n`)
    const prepared: PreparedSetup = hit ? { ...hit, cached: true } : await runAndExport(def, config, wasm, key)
    await recordEntry(cacheDir, def, testFile, key, parts)
    return { ...prepared, explanation }
  }
  return runAndExport(def, config, wasm, key)
}

async function runAndExport(def: AnySuite, config: Config, wasm: Buffer[], key: string): Promise<PreparedSetup> {
  const { cache, cacheDir } = config.sim
  const worker = await Worker.init({ config: { ...config, environment: 'soroban-sim' }, snapshot: config.sim.snapshot, seed: SEED })
  pairBindings(def, worker, wasm)
  let stored: StoredSetup
  let snapshot: unknown
  try {
    stored = await runSetup(def, worker, wasm)
    snapshot = await worker[ADMIN]('exportSnapshot', 'fork_exportSnapshot', {})
  } finally {
    await worker.tearDown()
  }
  const text = JSON.stringify(snapshot)
  if (cache === 'on') return { ...(await writeEntry(cacheDir, key, text, stored)), cached: false }
  const file = join(scratchDir(), `${def.name.replace(/[^\w.-]+/g, '_')}-${process.pid}-${Date.now()}.json`)
  await writeFile(file, text)
  return { ...stored, snapshot: file, cached: false }
}

function simEnvironment<S>(def: SuiteDefinition<S>, config: Config): Environment<S> {
  let prepared: Promise<PreparedSetup> | undefined
  let wasm: Promise<Buffer[]> | undefined
  return {
    async acquire(_testId, testFile) {
      // Setup runs (or comes from the cache) once, for the first test to ask.
      prepared ??= prepareSimSetup(def as AnySuite, config, testFile)
      wasm ??= loadWasm(def as AnySuite)
      const setup = await prepared
      const worker = await Worker.init({ config, snapshot: setup.snapshot, seed: SEED })
      try {
        pairBindings(def as AnySuite, worker, await wasm)
        return { worker, setup: await revive<S>(setup, worker), release: () => worker.tearDown() }
      } catch (e) {
        await worker.tearDown()
        throw e
      }
    },
  }
}

// --- local network ---

function localEnvironment<S>(def: SuiteDefinition<S>, config: Config): Environment<S> {
  let wasm: Promise<Buffer[]> | undefined
  let attempt = 0
  return {
    async acquire(testId) {
      wasm ??= loadWasm(def as AnySuite)
      // A local network keeps every account, so each test, and each run,
      // derives keys of its own: tests share no accounts and no contracts.
      const seed = `${SEED}/local/${RUN_NONCE}/${def.name}/${testId}/${attempt++}`
      const worker = await Worker.init({ config, seed })
      const code = await wasm
      pairBindings(def as AnySuite, worker, code)
      const stored = await runSetup(def as AnySuite, worker, code)
      return { worker, setup: await revive<S>(stored, worker), release: () => worker.tearDown() }
    },
  }
}
