// The sandbox controls: on soroban-sim they set balances, patch storage and
// move time; on a local network each refuses with SandboxOnlyError, and
// `sandbox` lets a suite skip the tests that need them.
import { expect } from 'vitest'
import { InvocationError, SandboxOnlyError, defineSuite } from '../src/index.ts'

const storageWasm = new URL('../../../fixtures/contracts/storage.wasm', import.meta.url)
const u32 = { keyType: 'u32', valueType: 'u32' }

const { test, sandbox } = defineSuite({
  name: 'sandbox controls',
  wasm: [storageWasm],
  async setup(worker) {
    const alice = await worker.createAccount('alice')
    const storage = await alice.deploy(storageWasm)
    await (await storage.put_persist({ key: 1, val: 7 })).signAndSend()
    return { alice, storage }
  },
})

test.skipIf(!sandbox)('setBalance sets an account’s XLM balance', async ({ setup: { alice } }) => {
  await alice.setBalance(5_0000000n)
  expect(await alice.balance()).toBe(5_0000000n)
})

test.skipIf(!sandbox)('setStorage and removeStorage patch what the contract reads', async ({ worker, setup: { storage } }) => {
  expect(await worker.getStorage(storage, 1, u32)).toBe(7)
  await worker.setStorage(storage, 1, 99, u32)
  await worker.setStorage(storage, 5, 50, u32)
  await worker.setStorage(storage, 3, 30, { ...u32, durability: 'temporary' })
  expect((await storage.get_persist({ key: 1 })).result).toBe(99)
  expect((await storage.get_persist({ key: 5 })).result).toBe(50)
  expect((await storage.get_temp({ key: 3 })).result).toBe(30)
  await worker.removeStorage(storage, 1, u32)
  expect(await worker.getStorage(storage, 1, u32)).toBeUndefined()
  const read = await storage.get_persist({ key: 1 })
  expect(() => read.result).toThrow(InvocationError)
})

test.skipIf(!sandbox)('advanceLedgers closes ledgers and setTime moves the clock', async ({ worker, setup: { storage } }) => {
  const before = await worker.latestLedger()
  const next = await worker.advanceLedgers(5)
  const after = await worker.latestLedger()
  expect(after.sequence).toBe(before.sequence + 5)
  expect(next.ledger).toBe(after.sequence + 1)

  const target = after.closeTime + 86_400
  expect((await worker.setTime(target)).closeTime).toBe(target)
  const sent = await (await storage.put_persist({ key: 2, val: 2 })).signAndSend()
  expect(Number(sent.getTransactionResponse?.createdAt)).toBe(target)
})

test.skipIf(sandbox)('on a local network every sandbox control refuses', async ({ worker, setup: { alice, storage } }) => {
  expect(worker.sandbox).toBe(false)
  const refusals = [
    () => alice.setBalance(1n),
    () => worker.setStorage(storage, 1, 1, u32),
    () => worker.removeStorage(storage, 1, u32),
    () => worker.advanceLedgers(1),
    () => worker.setTime(new Date()),
  ]
  for (const control of refusals) await expect(control()).rejects.toBeInstanceOf(SandboxOnlyError)
  expect((await storage.get_persist({ key: 1 })).result).toBe(7)
})
