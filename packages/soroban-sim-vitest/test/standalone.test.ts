// `@theahaco/soroban-sim-vitest/worker` loads, and starts a worker, in plain
// Node: no Vitest suite is needed, so scripts and global setup can use it.
import { execFile } from 'node:child_process'
import { fileURLToPath } from 'node:url'
import { promisify } from 'node:util'
import { expect, test } from 'vitest'

const run = promisify(execFile)
const packageDir = fileURLToPath(new URL('..', import.meta.url))

test('the worker entry point loads outside Vitest', async () => {
  const script = `const m = await import('@theahaco/soroban-sim-vitest/worker'); console.log(typeof m.Worker, typeof m.defineContract, 'defineSuite' in m)`
  const { stdout } = await run(process.execPath, ['--input-type=module', '-e', script], { cwd: packageDir })
  expect(stdout.trim()).toBe('function function false')
})

test('a worker started outside Vitest funds an account', async () => {
  const script = `
    const { Worker } = await import('@theahaco/soroban-sim-vitest/worker')
    const worker = await Worker.init()
    const alice = await worker.createAccount('alice')
    console.log(String(await alice.balance()))
    await worker.tearDown()`
  const { stdout } = await run(process.execPath, ['--input-type=module', '-e', script], { cwd: packageDir, env: process.env })
  expect(stdout.trim()).toBe('100000000000')
})

test('a worker started outside Vitest gives the asset contract its generated client', async () => {
  const script = `
    const { Worker } = await import('@theahaco/soroban-sim-vitest/worker')
    const worker = await Worker.init()
    const alice = await worker.createAccount('alice')
    const xlm = alice.client(await worker.xlm())
    const read = await xlm.balance({ id: alice.address })
    console.log(String(read.result), String(xlm.fromJSON.balance(read.toJSON()).result))
    await worker.tearDown()`
  const { stdout } = await run(process.execPath, ['--input-type=module', '-e', script], { cwd: packageDir, env: process.env })
  expect(stdout.trim()).toBe('100000000000 100000000000')
})
