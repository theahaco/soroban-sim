import { existsSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { defineConfig } from 'vitest/config'

// In this repository, use the workspace's own build unless SOROBAN_SIM_BIN says otherwise.
const built = ['release', 'debug']
  .map((profile) => fileURLToPath(new URL(`../../target/${profile}/soroban-sim`, import.meta.url)))
  .find((bin) => existsSync(bin))

export default defineConfig({
  test: {
    include: ['test/**/*.test.ts'],
    // Each test has an environment of its own, so a file's tests run at once.
    sequence: { concurrent: true },
    testTimeout: 120_000,
    env: !process.env.SOROBAN_SIM_BIN && built ? { SOROBAN_SIM_BIN: built } : {},
  },
})
