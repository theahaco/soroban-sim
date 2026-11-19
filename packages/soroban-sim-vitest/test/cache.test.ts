// The harness's own tests of the soroban-sim setup cache. They need the
// soroban-sim binary, so they run in the soroban-sim environment only.
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { describe, expect, test } from 'vitest'
import { cacheKey } from '../src/cache.ts'
import { Worker, clearCache, defineContract, prepareSimSetup, resolveConfig, type Config, type SuiteDefinition } from '../src/index.ts'

const env = resolveConfig()
const storage = new URL('../../../fixtures/contracts/storage.wasm', import.meta.url)
const addI32 = new URL('../../../fixtures/contracts/add_i32.wasm', import.meta.url)

/** Two versions of one suite's setup, counting their runs: the second differs in source only. */
function suites() {
  const counter = { runs: 0 }
  const base = { name: 'cache', wasm: [storage] }
  const first: SuiteDefinition<{ value: number }> = {
    ...base,
    async setup() {
      counter.runs++
      return { value: 1 }
    },
  }
  const changed: SuiteDefinition<{ value: number }> = {
    ...base,
    async setup() {
      counter.runs++
      return { value: 2 }
    },
  }
  return { first, changed, counter }
}

async function config(cache: Config['sim']['cache'] = 'on', cacheDir?: string): Promise<Config> {
  return { ...env, sim: { ...env.sim, cache, cacheDir: cacheDir ?? (await mkdtemp(join(tmpdir(), 'svt-cache-'))) } }
}

describe.skipIf(env.environment !== 'soroban-sim')('setup cache', () => {
  test('a second run takes the snapshot from the cache and does not run setup', async () => {
    const cfg = await config()
    const { first, counter } = suites()
    const a = await prepareSimSetup(first, cfg)
    expect([a.cached, counter.runs, a.result]).toEqual([false, 1, { value: 1 }])
    const b = await prepareSimSetup(first, cfg)
    expect([b.cached, counter.runs, b.result]).toEqual([true, 1, { value: 1 }])
    expect(b.snapshot).toBe(a.snapshot)
    const snap = JSON.parse(await readFile(b.snapshot, 'utf8'))
    expect(snap.format).toBe('soroban-sim-snapshot/2')
  })

  test('changing setup runs it again', async () => {
    const cfg = await config()
    const { first, changed, counter } = suites()
    await prepareSimSetup(first, cfg)
    const c = await prepareSimSetup(changed, cfg)
    expect([c.cached, counter.runs, c.result]).toEqual([false, 2, { value: 2 }])
  })

  test('SOROBAN_TEST_CACHE=off runs setup and stores nothing; clearCache empties the cache', async () => {
    const dir = (await config()).sim.cacheDir
    const { first, counter } = suites()
    await prepareSimSetup(first, await config('off', dir))
    await prepareSimSetup(first, await config('off', dir))
    expect(counter.runs).toBe(2)
    expect((await prepareSimSetup(first, await config('on', dir))).cached).toBe(false)
    expect((await prepareSimSetup(first, await config('on', dir))).cached).toBe(true)
    await clearCache(dir)
    expect((await prepareSimSetup(first, await config('on', dir))).cached).toBe(false)
    expect(counter.runs).toBe(4)
  })

  test('changing the test file that defines the suite runs setup again', async () => {
    const cfg = await config()
    const { first, counter } = suites()
    const testFile = join(cfg.sim.cacheDir, 'suite.test.ts')
    await writeFile(testFile, 'const INITIAL = 7\n')
    await prepareSimSetup(first, cfg, testFile)
    expect([(await prepareSimSetup(first, cfg, testFile)).cached, counter.runs]).toEqual([true, 1])
    await writeFile(testFile, 'const INITIAL = 8\n')
    expect([(await prepareSimSetup(first, cfg, testFile)).cached, counter.runs]).toEqual([false, 2])
    expect([(await prepareSimSetup(first, cfg, testFile)).cached, counter.runs]).toEqual([true, 2])
  })

  test('an entry that cannot be read fails the run, naming the cache to clear', async () => {
    const damage = [
      (entry: string) => rm(join(entry, 'snapshot.json')),
      (entry: string) => writeFile(join(entry, 'setup.json'), 'not json'),
    ]
    for (const harm of damage) {
      const cfg = await config()
      const { first } = suites()
      const a = await prepareSimSetup(first, cfg)
      await harm(dirname(a.snapshot))
      const error: Error = await prepareSimSetup(first, cfg).catch((e) => e)
      expect(error.message).toContain(`${dirname(a.snapshot)} cannot be read`)
      expect(error.message).toContain(`delete ${cfg.sim.cacheDir}, or call clearCache`)
      await clearCache(cfg.sim.cacheDir)
      expect((await prepareSimSetup(first, cfg)).cached).toBe(false)
      expect((await prepareSimSetup(first, cfg)).cached).toBe(true)
    }
  })

  test('the key covers the Wasm, the suite name and the base snapshot', async () => {
    const cfg = await config()
    const { first } = suites()
    const wasm = [await readFile(storage)]
    const key = async (def = first, c = cfg, w = wasm) => (await cacheKey(def as never, c, w)).key
    const original = await key()
    expect(await key()).toBe(original)
    expect(await key(first, cfg, [Buffer.concat([wasm[0], Buffer.from([0])])])).not.toBe(original)
    expect(await key({ ...first, name: 'other' })).not.toBe(original)
    const other = join(cfg.sim.cacheDir, 'other-base.json')
    await writeFile(other, `${await readFile(cfg.sim.snapshot, 'utf8')}\n`)
    expect(await key(first, { ...cfg, sim: { ...cfg.sim, snapshot: other } })).not.toBe(original)
  })

  test('a worker started from the snapshot holds what setup wrote, accounts and contracts included', async () => {
    const cfg = await config()
    const prepared = await prepareSimSetup(
      {
        name: 'state',
        wasm: [storage],
        async setup(worker) {
          const admin = await worker.createAccount('admin')
          await admin.setBalance(12_5000000n)
          const contract = await admin.deploy(storage)
          await (await contract.put_persist({ key: 1, val: 7 })).signAndSend()
          return { admin, contract }
        },
      },
      cfg,
    )
    expect(prepared.result).toMatchObject({
      admin: { name: 'admin' },
      contract: { id: expect.stringMatching(/^C/), signer: 'admin' },
    })
    const worker = await Worker.init({ config: cfg, snapshot: prepared.snapshot })
    try {
      const { contract } = prepared.result as { contract: { id: string } }
      expect((await (await worker.contract(contract.id)).get_persist({ key: 1 })).result).toBe(7)
    } finally {
      await worker.tearDown()
    }
  })

  test('setup may deploy only the Wasm the suite lists, so the key covers it', async () => {
    const cfg = await config()
    const suite: SuiteDefinition<undefined> = {
      name: 'unlisted',
      wasm: [storage],
      async setup(worker) {
        const admin = await worker.createAccount('admin')
        await admin.deploy(addI32)
      },
    }
    await expect(prepareSimSetup(suite, cfg)).rejects.toThrow(/does not list in `wasm`/)
  })

  test('setup may use a declaration with a generated client only when the suite lists that declaration', async () => {
    const cfg = await config()
    const { Client: StorageClient } = await import('./bindings/storage.ts')
    class OtherClient extends StorageClient {}
    const Storage = defineContract(storage, StorageClient)
    const refused = /declaration with a generated client for .*storage\.wasm, which the suite does not list in `wasm`: list that declaration/
    const deploys = (wasm: SuiteDefinition<undefined>['wasm']): SuiteDefinition<undefined> => ({
      name: 'unpaired',
      wasm,
      async setup(worker) {
        const admin = await worker.createAccount('admin')
        await admin.deploy(Storage)
      },
    })
    await expect(prepareSimSetup(deploys([storage]), cfg)).rejects.toThrow(refused)
    await expect(prepareSimSetup(deploys([defineContract(storage, OtherClient)]), cfg)).rejects.toThrow(refused)
    expect((await prepareSimSetup(deploys([Storage]), cfg)).cached).toBe(false)

    const attaches: SuiteDefinition<undefined> = {
      name: 'unpaired attach',
      wasm: [storage],
      async setup(worker) {
        const admin = await worker.createAccount('admin')
        const deployed = await admin.deploy(storage)
        await worker.contract(deployed.options.contractId, Storage)
      },
    }
    await expect(prepareSimSetup(attaches, cfg)).rejects.toThrow(/declaration with a generated client for C\w+, which the suite does not list/)
  })

  test('cacheInputs are part of the key, each by name', async () => {
    const cfg = await config()
    const wasm = [await readFile(storage)]
    const suite = (inputs?: Record<string, unknown>) => ({ ...suites().first, cacheInputs: inputs })
    const key = async (inputs?: Record<string, unknown>) => (await cacheKey(suite(inputs) as never, cfg, wasm)).key
    const supply = await key({ initialSupply: 1_000n, featureFlag: 'on' })
    expect(await key({ initialSupply: 1_000n, featureFlag: 'on' })).toBe(supply)
    expect(await key({ initialSupply: 2_000n, featureFlag: 'on' })).not.toBe(supply)
    expect(await key({ initialSupply: 1_000n, featureFlag: 'off' })).not.toBe(supply)
    expect(await key()).not.toBe(supply)
    await expect(key({ when: new Date(0) })).rejects.toThrow(/^cacheInputs\.when is a Date/)
  })

  test('with explain on, a run says whether setup came from the cache and what changed', async () => {
    const base = await config()
    const cfg = { ...base, sim: { ...base.sim, explain: true } }
    const { first } = suites()
    const withSupply = (initialSupply: bigint) => ({ ...first, cacheInputs: { initialSupply } })
    const explained = async (def: SuiteDefinition<{ value: number }>) => (await prepareSimSetup(def, cfg)).explanation
    expect(await explained(withSupply(1n))).toMatch(/cache miss for "cache" .*: no earlier entry for this suite$/)
    expect(await explained(withSupply(1n))).toMatch(/cache hit for "cache" .*: setup did not run$/)
    expect(await explained(withSupply(2n))).toMatch(/cache miss for "cache" .*: changed since the last entry: cacheInputs\.initialSupply$/)
  })

  test('with explain on, a setup that failed leaves no entry for the next run to compare with', async () => {
    const base = await config()
    const cfg = { ...base, sim: { ...base.sim, explain: true } }
    let fail = true
    const suite: SuiteDefinition<number> = {
      name: 'fails once',
      async setup() {
        if (fail) throw new Error('setup broke')
        return 1
      },
    }
    await expect(prepareSimSetup(suite, cfg)).rejects.toThrow('setup broke')
    fail = false
    expect((await prepareSimSetup(suite, cfg)).explanation).toMatch(/cache miss for "fails once" .*: no earlier entry for this suite$/)
    expect((await prepareSimSetup(suite, cfg)).explanation).toMatch(/cache hit for "fails once" /)
  })
})
