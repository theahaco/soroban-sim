// The example suite, without generated bindings: each contract client is
// the SDK's contract.Client built from the contract's spec. Nothing here
// names an environment: `pnpm test` runs it on soroban-sim, `pnpm
// test:local` against a local network.
import { contract } from '@stellar/stellar-sdk'
import { expect } from 'vitest'
import { ContractError, InvocationError, defineSuite } from '../src/index.ts'

const storageWasm = new URL('../../../fixtures/contracts/storage.wasm', import.meta.url)

const { test } = defineSuite({
  name: 'storage example',
  wasm: [storageWasm],
  async setup(worker) {
    const admin = await worker.createAccount('admin')
    const alice = await worker.createAccount('alice')
    const bob = await worker.createAccount('bob')
    const storage = await admin.deploy(storageWasm)
    await (await storage.put_persist({ key: 1, val: 7 })).signAndSend()
    return { admin, alice, bob, storage }
  },
})

test('reads what setup wrote', async ({ setup: { storage } }) => {
  expect((await storage.get_persist({ key: 1 })).result).toBe(7)
})

test('another account writes and reads its own value back', async ({ setup: { alice, storage } }) => {
  const asAlice = alice.client(storage)
  await (await asAlice.put_persist({ key: 2, val: 42 })).signAndSend()
  expect((await asAlice.get_persist({ key: 2 })).result).toBe(42)
})

test('accounts setup did not use still hold their funding', async ({ setup: { alice, bob } }) => {
  expect(await alice.balance()).toBe(10_000_0000000n)
  expect(await bob.balance()).toBe(10_000_0000000n)
})

test('a test deploys a contract of its own', async ({ setup: { bob, storage } }) => {
  const mine = await bob.deploy(storageWasm)
  expect(mine.options.contractId).not.toBe(storage.options.contractId)
  await (await mine.put_persist({ key: 1, val: 9 })).signAndSend()
  expect((await mine.get_persist({ key: 1 })).result).toBe(9)
  expect((await storage.get_persist({ key: 1 })).result).toBe(7)
})

test('XLM moves between accounts through the native asset contract', async ({ worker, setup: { alice, bob } }) => {
  const xlm = alice.client(await worker.xlm())
  await (await xlm.transfer({ from: alice.address, to: bob.address, amount: 25_0000000n })).signAndSend()
  expect((await xlm.balance({ id: bob.address })).result).toBe(10_025_0000000n)
})

test('a transfer another account authorizes uses the SDK’s signAuthEntries', async ({ worker, setup: { alice, bob } }) => {
  const xlm = bob.client(await worker.xlm())
  const tx = await xlm.transfer({ from: alice.address, to: bob.address, amount: 5_0000000n })
  expect(tx.needsNonInvokerSigningBy()).toEqual([alice.address])
  await alice.signAuthEntries(tx)
  await tx.signAndSend()
  expect((await xlm.balance({ id: alice.address })).result).toBe(9_995_0000000n)
})

test("a contract's own error is thrown by signAndSend as a ContractError with its code", async ({ worker, setup: { alice, bob } }) => {
  const xlm = alice.client(await worker.xlm())
  const tx = await xlm.transfer({ from: alice.address, to: bob.address, amount: 1_000_000_0000000n })
  const error = await tx.signAndSend().catch((e: any) => e)
  expect(error).toBeInstanceOf(ContractError)
  expect(error).toMatchObject({ code: 10, errorName: 'BalanceError', method: 'transfer', contract: xlm.options.contractId })
  expect(error.events.length).toBeGreaterThan(0)
  expect(error.cause).toBeInstanceOf(contract.AssembledTransaction.Errors.SimulationFailed)
})

test('a read that traps throws an InvocationError from result, with the host error and its events', async ({ setup: { storage } }) => {
  const tx = await storage.get_persist({ key: 99 })
  let error: any
  try {
    void tx.result
  } catch (e) {
    error = e
  }
  expect(error).toBeInstanceOf(InvocationError)
  expect(error).not.toBeInstanceOf(ContractError)
  expect(error.hostError).toMatch(/^Error\(WasmVm, /)
  expect(error.events.length).toBeGreaterThan(0)
})
