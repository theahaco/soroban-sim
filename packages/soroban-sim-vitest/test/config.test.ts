import { describe, expect, test } from 'vitest'
import { MAX_WAIT_SECONDS, WAIT_MARGIN_MS, resolveConfig } from '../src/config.ts'
import { deriveKeypair } from '../src/keys.ts'

describe('account keys', () => {
  test('the same seed and name give the same key; another seed or name another key', () => {
    const admin = deriveKeypair('seed', 'admin')
    expect(deriveKeypair('seed', 'admin').secret()).toBe(admin.secret())
    expect(deriveKeypair('seed', 'alice').publicKey()).not.toBe(admin.publicKey())
    expect(deriveKeypair('other', 'admin').publicKey()).not.toBe(admin.publicKey())
  })
})

describe('configuration', () => {
  test('defaults to soroban-sim over the bundled local-network snapshot, with the cache on', () => {
    const c = resolveConfig({})
    expect(c.environment).toBe('soroban-sim')
    expect(c.sim.cache).toBe('on')
    expect(c.sim.snapshot).toMatch(/snapshots\/local-network\.json$/)
    expect(c.local).toEqual({ rpcUrl: 'http://localhost:8000/rpc', friendbotUrl: 'http://localhost:8000/friendbot' })
  })

  test('reads the environment, snapshot and cache from variables', () => {
    const c = resolveConfig({
      SOROBAN_TEST_ENV: 'local',
      SOROBAN_SIM_SNAPSHOT: '/tmp/base.json',
      SOROBAN_TEST_CACHE: 'off',
      SOROBAN_TEST_RPC_URL: 'http://127.0.0.1:8010/rpc',
    })
    expect(c.environment).toBe('local')
    expect(c.sim.snapshot).toBe('/tmp/base.json')
    expect(c.sim.cache).toBe('off')
    expect(c.local.friendbotUrl).toBe('http://127.0.0.1:8010/friendbot')
  })

  test('reads the wait bound and the cache explanation switch', () => {
    expect(resolveConfig({}).waitSeconds).toBe(60)
    expect(resolveConfig({}).sim.explain).toBe(false)
    const c = resolveConfig({ SOROBAN_TEST_WAIT_SECONDS: '15', SOROBAN_TEST_CACHE_EXPLAIN: 'on' })
    expect([c.waitSeconds, c.sim.explain]).toEqual([15, true])
    expect(resolveConfig({ SOROBAN_TEST_CACHE_EXPLAIN: 'off' }).sim.explain).toBe(false)
    for (const refused of ['1', 'true']) {
      expect(() => resolveConfig({ SOROBAN_TEST_CACHE_EXPLAIN: refused }), refused).toThrow(
        `SOROBAN_TEST_CACHE_EXPLAIN must be "on" or "off", not "${refused}"`,
      )
    }
  })

  test('the wait bound is a finite number of seconds above 0 that a timer can hold', () => {
    const waitSeconds = (value: string) => resolveConfig({ SOROBAN_TEST_WAIT_SECONDS: value }).waitSeconds
    expect(waitSeconds('0.5')).toBe(0.5)
    expect(waitSeconds(String(MAX_WAIT_SECONDS))).toBe(MAX_WAIT_SECONDS)
    expect(MAX_WAIT_SECONDS * 1000 + WAIT_MARGIN_MS).toBeLessThanOrEqual(2 ** 31 - 1)
    for (const refused of ['soon', '0', '-5', 'Infinity', '3000000', String(MAX_WAIT_SECONDS + 1)]) {
      expect(() => waitSeconds(refused), refused).toThrow(
        `SOROBAN_TEST_WAIT_SECONDS must be a number of seconds above 0 and at most ${MAX_WAIT_SECONDS} (the longest a timer can run), not "${refused}"`,
      )
    }
  })

  test('refuses values it does not know', () => {
    expect(() => resolveConfig({ SOROBAN_TEST_ENV: 'testnet' })).toThrow(/SOROBAN_TEST_ENV/)
    expect(() => resolveConfig({ SOROBAN_TEST_CACHE: 'no' })).toThrow(/SOROBAN_TEST_CACHE/)
    expect(() => resolveConfig({ SOROBAN_TEST_CACHE: 'refresh' })).toThrow(/SOROBAN_TEST_CACHE/)
  })
})
