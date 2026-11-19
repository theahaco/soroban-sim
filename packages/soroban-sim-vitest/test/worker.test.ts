// The plain `test`: a worker of the test's own, no suite setup.
import { Asset, contract } from '@stellar/stellar-sdk'
import { expect } from 'vitest'
import { InvocationError, test } from '../src/index.ts'

const storageWasm = new URL('../../../fixtures/contracts/storage.wasm', import.meta.url)
const verifierWasm = new URL('../../../fixtures/contracts/verifier.wasm', import.meta.url)
// A module with a contract spec (one empty error enum) and nothing else: the host refuses to upload it.
const emptyErrorEnum = [0, 0, 0, 4, ...new Array(16).fill(0)]
const refusedWasm = Uint8Array.from([0, 0x61, 0x73, 0x6d, 1, 0, 0, 0, 0, 35, 14, ...Buffer.from('contractspecv0'), ...emptyErrorEnum])

test('accounts by name deploy, write and read through SDK clients', async ({ worker }) => {
  const alice = await worker.createAccount('alice')
  const storage = await alice.deploy(storageWasm)
  expect(storage).toBeInstanceOf(contract.Client)
  await (await storage.put_persist({ key: 1, val: 3 })).signAndSend()
  expect((await storage.get_persist({ key: 1 })).result).toBe(3)
  expect(worker.account('alice')).toBe(alice)
})

test('worker.contract finds a contract’s spec on the network and reads without a signer', async ({ worker }) => {
  const alice = await worker.createAccount('alice')
  const deployed = await alice.deploy(storageWasm)
  await (await deployed.put_persist({ key: 1, val: 4 })).signAndSend()
  const found = await worker.contract(deployed.options.contractId)
  expect(found.options.publicKey).toBeUndefined()
  expect((await found.get_persist({ key: 1 })).result).toBe(4)
})

test('a name is one account: creating it twice is refused, as is an unknown name', async ({ worker }) => {
  await worker.createAccount('alice')
  await expect(worker.createAccount('alice')).rejects.toThrow(/already exists/)
  expect(() => worker.account('carol')).toThrow(/no account "carol"/)
})

test('a Stellar asset contract takes its admin functions by name', async ({ worker }) => {
  const issuer = await worker.createAccount('issuer')
  const token = issuer.client(await worker.assetContract(new Asset('TST', issuer.address)))
  const holder = await issuer.deploy(storageWasm)
  const id = holder.options.contractId
  await (await token.mint({ to: id, amount: 5n })).signAndSend()
  expect((await token.balance({ id })).result).toBe(5n)
  expect((await token.admin()).result).toBe(issuer.address)
  expect((await token.authorized({ id })).result).toBe(true)
})

test('one worker deploys several asset contracts, also when asked for them at once', async ({ worker }) => {
  const issuer = await worker.createAccount('issuer')
  const first = await worker.assetContract(new Asset('ONE', issuer.address))
  const [second, third] = await Promise.all([
    worker.assetContract(new Asset('TWO', issuer.address)),
    worker.assetContract(new Asset('THREE', issuer.address)),
  ])
  for (const token of [first, second, third]) {
    expect((await token.admin()).result).toBe(issuer.address)
  }
})

test('a deploy whose constructor traps throws an InvocationError', async ({ worker }) => {
  const alice = await worker.createAccount('alice')
  const error = await alice.deploy(verifierWasm).catch((e) => e)
  expect(error).toBeInstanceOf(InvocationError)
  expect(error.hostError).toMatch(/^Error\(/)
})

test('a deploy of code the host refuses to upload throws an InvocationError', async ({ worker }) => {
  const alice = await worker.createAccount('alice')
  await expect(alice.deploy(refusedWasm)).rejects.toBeInstanceOf(InvocationError)
})
