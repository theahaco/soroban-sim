// Setup's result reaches each test with the values contracts use: bigints
// and bytes survive, on both environments and from the cache; a value that
// cannot be stored is refused with where it is.
import { describe, expect } from 'vitest'
import { test as plain } from 'vitest'
import { defineSuite } from '../src/index.ts'
import { encode } from '../src/setup-values.ts'

const { test } = defineSuite({
  name: 'setup values',
  async setup(worker) {
    const alice = await worker.createAccount('alice')
    return {
      amount: 10_000_0000000n,
      negative: [-5n, { deep: 2n ** 100n }],
      hash: new Uint8Array([0, 1, 2, 254, 255]),
      owner: alice,
      label: 'x',
    }
  },
})

test('bigints and bytes come back as bigints and bytes', async ({ setup }) => {
  expect(setup.amount).toBe(10_000_0000000n)
  expect(setup.negative).toEqual([-5n, { deep: 2n ** 100n }])
  expect(setup.hash).toBeInstanceOf(Uint8Array)
  expect([...setup.hash]).toEqual([0, 1, 2, 254, 255])
  expect(setup.label).toBe('x')
  expect(await setup.owner.balance()).toBe(10_000_0000000n)
})

describe('values that cannot be stored are refused with their path', () => {
  const cases: [string, unknown, RegExp][] = [
    ['a Map', { balances: [1, new Map()] }, /^result\.balances\[1\] is a Map/],
    ['a Date', { at: new Date(0) }, /^result\.at is a Date/],
    ['a function', { fn: () => 1 }, /^result\.fn is function/],
    ['NaN', { ratio: Number.NaN }, /^result\.ratio is NaN/],
    ['undefined in an array', { list: [1, undefined] }, /^result\.list\[1\] is undefined/],
    ['an odd key', { 'a b': Symbol('s') }, /^result\["a b"\] is symbol/],
  ]
  for (const [what, value, message] of cases) {
    plain(what, () => {
      expect(() => encode(value)).toThrow(message)
    })
  }
})
