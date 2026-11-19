// The same storage suite with the client `stellar contract bindings
// typescript` generated for the contract, declared once with
// defineContract: methods and results are typed wherever it is used.
import { contract } from '@stellar/stellar-sdk'
import { expect, expectTypeOf } from 'vitest'
import { defineContract, defineSuite, send } from '../src/index.ts'
import { Client as StorageClient } from './bindings/storage.ts'

const Storage = defineContract(new URL('../../../fixtures/contracts/storage.wasm', import.meta.url), StorageClient)

const { test } = defineSuite({
  name: 'storage with bindings',
  wasm: [Storage],
  async setup(worker) {
    const admin = await worker.createAccount('admin')
    const alice = await worker.createAccount('alice')
    const storage = await admin.deploy(Storage)
    await send(storage.put_persist({ key: 1, val: 7 }))
    return { alice, storage }
  },
})

test('deploy(Storage) gives the generated client, typed, and setup’s comes back as one', async ({ setup: { storage } }) => {
  expectTypeOf(storage).toEqualTypeOf<StorageClient>()
  expect(storage).toBeInstanceOf(StorageClient)
  const { result } = await storage.get_persist({ key: 1 })
  expectTypeOf(result).toEqualTypeOf<number>()
  expect(result).toBe(7)
})

test('another account writes and reads its own value back', async ({ setup: { alice, storage } }) => {
  const asAlice = alice.client(storage)
  expectTypeOf(asAlice).toEqualTypeOf<StorageClient>()
  const sent = await send(asAlice.put_persist({ key: 2, val: 42 }))
  expectTypeOf(sent).toEqualTypeOf<contract.SentTransaction<null>>()
  expect(sent).toBeInstanceOf(contract.SentTransaction)
  expect((await asAlice.get_persist({ key: 2 })).result).toBe(42)
})

test('a contract a test deploys, or attaches to, by its declaration is typed too', async ({ worker, setup: { alice } }) => {
  const mine = await alice.deploy(Storage)
  const found = await worker.contract(mine.options.contractId, Storage)
  expectTypeOf(found).toEqualTypeOf<StorageClient>()
  await send(mine.put_persist({ key: 3, val: 3 }))
  expect((await found.get_persist({ key: 3 })).result).toBe(3)
})

test('worker.xlm() is a typed token client', async ({ worker, setup: { alice } }) => {
  const xlm = alice.client(await worker.xlm())
  const { result } = await xlm.balance({ id: alice.address })
  expectTypeOf(result).toEqualTypeOf<bigint>()
  expect(result).toBe(10_000_0000000n)
})
