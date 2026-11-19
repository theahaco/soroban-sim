import { test as baseTest, type TestAPI } from 'vitest'
import { resolveConfig, type Config } from './config.ts'
import { environmentFor, type Lease, type SuiteDefinition } from './environments.ts'
import { Worker } from './worker.ts'

export interface Suite<S> {
  /**
   * Vitest's `test`, whose tests receive `worker` (a network of their own on
   * soroban-sim) and `setup` (their own copy of what the suite's setup
   * returned, its accounts and contracts on that network).
   */
  test: TestAPI<{ worker: Worker; setup: S }>
  /** True where the sandbox controls work (soroban-sim): skip tests that need them with test.skipIf(!sandbox). */
  sandbox: boolean
}

/**
 * Declares a suite: a setup that runs once on soroban-sim (cached across
 * runs) and once per test on a local network, and a `test` whose tests start
 * from it. Which environment runs is configuration (SOROBAN_TEST_ENV), never
 * part of the test code.
 */
export function defineSuite<S>(def: SuiteDefinition<S>, config: Config = resolveConfig()): Suite<S> {
  const environment = environmentFor(def, config)
  const test = baseTest.extend<{ lease: Lease<S>; worker: Worker; setup: S }>({
    lease: async ({ task }, use) => {
      const lease = await environment.acquire(task.id, task.file.filepath)
      try {
        await use(lease)
      } finally {
        await lease.release()
      }
    },
    worker: async ({ lease }, use) => use(lease.worker),
    // Annotated: S could be any type, a function's included, so Vitest cannot infer this fixture's shape.
    setup: async ({ lease }: { lease: Lease<S> }, use: (value: S) => Promise<void>) => use(lease.setup),
  })
  return { test: test as unknown as Suite<S>['test'], sandbox: config.environment === 'soroban-sim' }
}

/**
 * Vitest's `test` with a `worker` of the test's own and no suite setup: a
 * fresh soroban-sim server per test, or the local network with accounts no
 * other test has.
 */
export const test: TestAPI<{ worker: Worker }> = baseTest.extend<{ worker: Worker }>({
  // eslint-disable-next-line no-empty-pattern
  worker: async ({}, use) => {
    const worker = await Worker.init()
    try {
      await use(worker)
    } finally {
      await worker.tearDown()
    }
  },
})
