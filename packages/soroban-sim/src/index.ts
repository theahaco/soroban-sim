// Component boundary (jco-generated).
export {
  runFixture,
  simulate,
  simulateTransaction,
} from "./generated/soroban-sim-component.js";
export type {
  LedgerContext,
  LedgerEntry,
  Outcome,
  Request,
  TransactionRequest,
} from "./generated/soroban-sim-component.js";

// State provider contract (#8 ships concrete providers).
export type { EntryResolution, StateProvider } from "./state-provider.js";

// Loop safety limits + enforcement (#10).
export {
  assertEntrySize,
  DEFAULT_LIMITS,
  LimitTracker,
  SimulationLimitError,
} from "./limits.js";
export type { Limits, LimitViolation } from "./limits.js";

// Simulator driver (Topology A rerun loop).
export { SorobanSimulator } from "./simulator.js";
export type {
  Component,
  ComponentSimulate,
  ComponentSimulateTransaction,
  SimulateInput,
  SimulateProgress,
  SimulateResult,
  SimulateRunOptions,
  SimulateTransactionInput,
  SimulatorOptions,
  StateCache,
} from "./simulator.js";

// --- Intended stable export paths for later M2 stages ---------------------
// These files arrive in #8/#9; the lines are pre-declared here (commented) so
// those stages need not edit this barrel again — just uncomment.
//
// #8 StateProvider implementations:
export { InMemoryStateProvider } from "./providers/in-memory.js";
export type {
  InMemoryEntry,
  InMemoryStateProviderOptions,
} from "./providers/in-memory.js";
export { RpcStateProvider } from "./providers/rpc.js";
export type {
  LedgerEntriesServer,
  RpcStateProviderConfig,
} from "./providers/rpc.js";
//
// #9 IndexedDB cache:
export { CachingStateProvider } from "./cache/caching-provider.js";
export type {
  CacheContext,
  CachingStateProviderOptions,
} from "./cache/caching-provider.js";
export { CACHE_SCHEMA_VERSION, IndexedDbEntryCache } from "./cache/indexeddb.js";
export type {
  CacheAbsentRecord,
  CachePresentRecord,
  CacheRecord,
  IndexedDbEntryCacheOptions,
} from "./cache/indexeddb.js";
