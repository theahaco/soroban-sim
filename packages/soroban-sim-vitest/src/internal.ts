// Keys for what the package's own modules call on a Worker but tests do not.

/** `worker[ADMIN](capability, method, params)`: a soroban-sim admin call, refused on a local network. */
export const ADMIN = Symbol('soroban-sim-vitest/admin')
/** `worker[ADOPT](names)`: accounts that already exist in the state the worker started from. */
export const ADOPT = Symbol('soroban-sim-vitest/adopt')
/** `worker[ACCOUNTS]()`: the names of every account the worker knows. */
export const ACCOUNTS = Symbol('soroban-sim-vitest/accounts')
/** `worker[DEPLOYS]`: while setup runs, the hashes of the Wasm it may deploy. */
export const DEPLOYS = Symbol('soroban-sim-vitest/deploys')

/** The JSON key that marks an account or contract in a setup's stored result. */
export const MARKER = '@soroban-sim-vitest'
/** `worker[BINDINGS]`: generated bindings clients by the hash of the Wasm they are for, from the suite's `wasm`. */
export const BINDINGS = Symbol('soroban-sim-vitest/bindings')
/** `worker[CLOSED]`: aborted when the worker is torn down, cancelling any wait for a transaction. */
export const CLOSED = Symbol('soroban-sim-vitest/closed')
