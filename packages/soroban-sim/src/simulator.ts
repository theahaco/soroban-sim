/**
 * SorobanSimulator — Topology A driver for the jco-transpiled `simulate`
 * component export.
 *
 * The component simulate() is synchronous: given a Request it returns an Outcome
 * that is either `complete(bytes)` or `needs-state(keys)`. This class owns the
 * async rerun loop: on `needs-state` it asks a StateProvider to resolve the
 * missing LedgerKeys, folds present entries into `ledgerEntries` and absent keys
 * into `knownAbsentKeys`, and reruns. Known-absent keys are re-fed every round so
 * they never re-trigger needs-state (loop prevention). An attempted-set plus
 * `Limits.maxRounds` guarantee termination.
 */
import type {
  LedgerContext,
  LedgerEntry,
  Outcome,
  Request,
  TransactionRequest,
} from "./generated/soroban-sim-component.js";
import type { EntryResolution, StateProvider } from "./state-provider.js";
import {
  DEFAULT_LIMITS,
  type Limits,
  LimitTracker,
  SimulationLimitError,
} from "./limits.js";

/** The host-function component entry point this class drives. Mockable in tests. */
export type ComponentSimulate = (request: Request) => Outcome;
/** The transaction-envelope component entry point. Mockable in tests. */
export type ComponentSimulateTransaction = (
  request: TransactionRequest,
) => Outcome;
export interface Component {
  simulate: ComponentSimulate;
  /**
   * Transaction-envelope entry point. Optional so existing test doubles that
   * only script `simulate` keep type-checking; {@link SorobanSimulator.simulateTransaction}
   * throws if a component without it is used.
   */
  simulateTransaction?: ComponentSimulateTransaction;
}

/** Optional cache layer (M2 #9 supplies a concrete implementation). */
export interface StateCache {
  // Intentionally opaque at this stage; #9 defines the concrete contract via
  // CachingStateProvider, which wraps a StateProvider rather than plugging here.
  readonly [key: string]: unknown;
}

export interface SimulatorOptions {
  /** Stellar network passphrase (identifies the network for providers/caches). */
  networkPassphrase: string;
  /** RPC endpoint; #8's RpcStateProvider is constructed from this when no explicit provider is given. */
  rpcUrl?: string;
  /** Explicit StateProvider. Takes precedence over rpcUrl. */
  provider?: StateProvider;
  /** Optional cache (reserved for #9). */
  cache?: StateCache;
  /** Loop safety limits; merged over DEFAULT_LIMITS. */
  limits?: Partial<Limits>;
  /**
   * Dependency-injected component. Defaults to a dynamic import of
   * `./generated`. Tests pass a mock here to exercise the loop without the wasm.
   */
  component?: Component;
}

export interface SimulateInput {
  /** stellar-xdr HostFunction bytes. */
  hostFunctionXdr: Uint8Array;
  /** stellar-xdr AccountId bytes. */
  sourceAccountXdr: Uint8Array;
  ledgerContext: LedgerContext;
  /** Entries the caller already knows are present (seeded into the first request). */
  seedEntries?: LedgerEntry[];
  basePrngSeed?: Uint8Array;
  enableDiagnostics?: boolean;
  /**
   * Apply soroban-simulation's RPC-parity resource/fee adjustment. Defaults to
   * false (raw metered resources/fees); set true to reproduce the padded
   * instructions/fees stellar-rpc's preflight returns.
   */
  applyRpcAdjustments?: boolean;
}

export interface SimulateTransactionInput {
  /** stellar-xdr TransactionEnvelope bytes. */
  transactionXdr: Uint8Array;
  ledgerContext: LedgerContext;
  /** Entries the caller already knows are present (seeded into the first request). */
  seedEntries?: LedgerEntry[];
  basePrngSeed?: Uint8Array;
  enableDiagnostics?: boolean;
  /**
   * Apply soroban-simulation's RPC-parity resource/fee adjustment. Defaults to
   * false (raw metered resources/fees); set true to reproduce the padded
   * instructions/fees stellar-rpc's preflight returns.
   */
  applyRpcAdjustments?: boolean;
}

export interface SimulateResult {
  /** Deterministic SSIM-encoded result bytes (outcome::complete). */
  resultXdr: Uint8Array;
  /** Number of component runs performed (>= 1). */
  rounds: number;
  /** Total distinct keys requested from the provider across all rounds. */
  requestedKeyCount: number;
}

export interface SimulateProgress {
  /** Number of reruns triggered by needs-state so far (component runs minus 1). */
  rerunCount: number;
  /** Total distinct keys requested from the provider so far. */
  requestedKeyCount: number;
  /** The batch of keys resolved in the round just completed. */
  lastMissingBatch: Uint8Array[];
}

export interface SimulateRunOptions {
  signal?: AbortSignal;
  onProgress?: (progress: SimulateProgress) => void;
}

const EMPTY = new Uint8Array(0);

function hex(bytes: Uint8Array): string {
  let out = "";
  for (const b of bytes) out += b.toString(16).padStart(2, "0");
  return out;
}

function mergeLimits(overrides?: Partial<Limits>): Limits {
  return { ...DEFAULT_LIMITS, ...overrides };
}

const abortError = () => new DOMException("simulate aborted", "AbortError");

/**
 * Resolve a batch through the provider while honoring cancellation and the
 * per-round timeout. Rejects with an AbortError if `signal` fires, with a
 * {@link SimulationLimitError} ("round-timeout") if `timeoutMs` elapses, or with
 * the provider's own error. Whichever settles first wins; the losers are
 * cleaned up. Note this can only interrupt the *async* provider call — a
 * synchronous component hang requires `Worker.terminate()` from the main thread.
 */
function resolveWithLimits(
  provider: StateProvider,
  batch: Uint8Array[],
  signal: AbortSignal | undefined,
  timeoutMs: number | undefined,
): Promise<EntryResolution[]> {
  return new Promise<EntryResolution[]>((resolve, reject) => {
    if (signal?.aborted) {
      reject(abortError());
      return;
    }
    let settled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const cleanup = () => {
      if (timer !== undefined) clearTimeout(timer);
      signal?.removeEventListener("abort", onAbort);
    };
    const finish = (fn: (v: never) => void, value: never) => {
      if (settled) return;
      settled = true;
      cleanup();
      fn(value);
    };
    function onAbort() {
      finish(reject as (v: never) => void, abortError() as never);
    }
    signal?.addEventListener("abort", onAbort);
    if (timeoutMs !== undefined && timeoutMs > 0) {
      timer = setTimeout(() => {
        finish(
          reject as (v: never) => void,
          new SimulationLimitError(
            "round-timeout",
            `provider.resolve exceeded roundTimeoutMs (${timeoutMs}ms)`,
          ) as never,
        );
      }, timeoutMs);
    }
    provider.resolve(batch).then(
      (r) => finish(resolve as (v: never) => void, r as never),
      (e) => finish(reject as (v: never) => void, e as never),
    );
  });
}

export class SorobanSimulator {
  readonly networkPassphrase: string;
  private readonly component: Component;
  private readonly provider?: StateProvider;
  private readonly limits: Limits;

  private constructor(args: {
    networkPassphrase: string;
    component: Component;
    provider?: StateProvider;
    limits: Limits;
  }) {
    this.networkPassphrase = args.networkPassphrase;
    this.component = args.component;
    this.provider = args.provider;
    this.limits = args.limits;
  }

  static async create(opts: SimulatorOptions): Promise<SorobanSimulator> {
    const component =
      opts.component ??
      ((await import("./generated/soroban-sim-component.js")) as unknown as Component);
    return new SorobanSimulator({
      networkPassphrase: opts.networkPassphrase,
      component,
      provider: opts.provider,
      limits: mergeLimits(opts.limits),
    });
  }

  /**
   * Drive the host-function `simulate` component export through the rerun loop.
   */
  async simulate(
    input: SimulateInput,
    runOpts: SimulateRunOptions = {},
  ): Promise<SimulateResult> {
    return this.runLoop(
      input.seedEntries,
      (ledgerEntries, knownAbsentKeys) =>
        this.component.simulate({
          hostFunctionXdr: input.hostFunctionXdr,
          sourceAccountXdr: input.sourceAccountXdr,
          ledgerContext: input.ledgerContext,
          ledgerEntries,
          knownAbsentKeys,
          basePrngSeed: input.basePrngSeed ?? EMPTY,
          enableDiagnostics: input.enableDiagnostics ?? false,
          applyRpcAdjustments: input.applyRpcAdjustments ?? false,
        }),
      runOpts,
    );
  }

  /**
   * Drive the transaction-envelope `simulate-transaction` component export
   * through the same rerun loop. The component extracts the InvokeHostFunction
   * host function + source account from the envelope.
   */
  async simulateTransaction(
    input: SimulateTransactionInput,
    runOpts: SimulateRunOptions = {},
  ): Promise<SimulateResult> {
    const runTx = this.component.simulateTransaction;
    if (!runTx) {
      throw new Error(
        "component does not export simulate-transaction; rebuild the wasm + jco bindings",
      );
    }
    return this.runLoop(
      input.seedEntries,
      (ledgerEntries, knownAbsentKeys) => {
        const request: TransactionRequest = {
          transactionXdr: input.transactionXdr,
          ledgerContext: input.ledgerContext,
          ledgerEntries,
          knownAbsentKeys,
          basePrngSeed: input.basePrngSeed ?? EMPTY,
          enableDiagnostics: input.enableDiagnostics ?? false,
          applyRpcAdjustments: input.applyRpcAdjustments ?? false,
        };
        return runTx(request);
      },
      runOpts,
    );
  }

  /**
   * Shared Topology-A rerun loop. `runComponent` builds and invokes the
   * appropriate component export for a round given the current present-entry and
   * known-absent-key sets; everything else (needs-state resolution, limits,
   * cancellation, progress) is identical across entry points.
   */
  private async runLoop(
    seedEntries: LedgerEntry[] | undefined,
    runComponent: (
      ledgerEntries: LedgerEntry[],
      knownAbsentKeys: Uint8Array[],
    ) => Outcome,
    runOpts: SimulateRunOptions,
  ): Promise<SimulateResult> {
    const { signal, onProgress } = runOpts;
    const throwIfAborted = () => {
      if (signal?.aborted) {
        throw new DOMException("simulate aborted", "AbortError");
      }
    };

    const ledgerEntries: LedgerEntry[] = seedEntries ? [...seedEntries] : [];
    const knownAbsentKeys: Uint8Array[] = [];
    const attempted = new Set<string>();

    // Central limit accounting. Seed entries count toward the size/entry caps.
    const tracker = new LimitTracker(this.limits);
    tracker.admitSeedEntries(ledgerEntries);

    let requestedKeyCount = 0;

    for (;;) {
      throwIfAborted();

      const rounds = tracker.enterRound();
      const outcome: Outcome = runComponent(ledgerEntries, knownAbsentKeys);

      if (outcome.tag === "complete") {
        return { resultXdr: outcome.val, rounds, requestedKeyCount };
      }

      // needs-state: resolve the un-attempted missing keys and rerun.
      const missing = outcome.val;
      const batch: Uint8Array[] = [];
      for (const key of missing) {
        if (!attempted.has(hex(key))) batch.push(key);
      }

      // Empty batch => stall (only already-attempted keys were re-requested).
      tracker.assertProgress(batch, missing.length);
      // Backstop before committing to another component run.
      tracker.assertCanRerun();

      if (!this.provider) {
        throw new Error(
          `simulate needs ${batch.length} more state entr(ies) but no StateProvider was configured`,
        );
      }

      throwIfAborted();
      const resolutions = await resolveWithLimits(
        this.provider,
        batch,
        signal,
        this.limits.roundTimeoutMs,
      );
      throwIfAborted();

      if (resolutions.length !== batch.length) {
        throw new Error(
          `StateProvider returned ${resolutions.length} resolutions for ${batch.length} keys (must be 1:1)`,
        );
      }

      for (let i = 0; i < batch.length; i += 1) {
        const key = batch[i]!;
        attempted.add(hex(key));
        requestedKeyCount += 1;
        const res = resolutions[i]!;
        if (res.status === "present") {
          // Enforces per-entry size + accumulated entry/byte caps.
          tracker.admitEntry(res.entry.entryXdr);
          ledgerEntries.push({
            entryXdr: res.entry.entryXdr,
            liveUntilLedger: res.entry.liveUntilLedger,
          });
        } else {
          knownAbsentKeys.push(key);
        }
      }

      onProgress?.({
        rerunCount: rounds,
        requestedKeyCount,
        lastMissingBatch: batch,
      });
    }
  }
}
