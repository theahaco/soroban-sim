/**
 * Central safety limits and enforcement for the missing-state rerun loop.
 *
 * ## Threat model / untrusted-input assumptions
 *
 * The simulator drives an untrusted wasm component (the transpiled Soroban host)
 * against ledger state supplied by an untrusted `StateProvider` (typically a
 * remote RPC endpoint). Neither is allowed to make the loop run unbounded work:
 *
 *   - A malicious or buggy component could emit `needs-state` forever, or emit
 *     a growing set of fresh keys each round, or re-request keys we already
 *     resolved. `maxRounds` + stall detection bound this.
 *   - A malicious provider could return arbitrarily large entry blobs to blow
 *     memory, or return more/fewer resolutions than requested, or hang forever.
 *     `maxBytesPerEntry`, `maxTotalBytes`, `maxTotalEntries` and `roundTimeoutMs`
 *     bound this (the 1:1 length check lives in the simulator loop).
 *
 * ## Termination invariant
 *
 * The loop terminates because of two cooperating mechanisms:
 *
 *   1. **attempted-set**: every key handed to the provider is recorded in a
 *      `Set<hex(keyXdr)>`. A key is asked for at most once.
 *   2. **known-absent-keys**: a key the provider reports absent is fed back into
 *      the next Request's `knownAbsentKeys`, so the component maps it to Ok(None)
 *      and never re-emits it as needs-state.
 *
 * Consequently a `needs-state` round that requests only already-attempted keys
 * is a STALL (no forward progress possible) and is rejected immediately rather
 * than looping. `maxRounds` is the final backstop for any case the attempted-set
 * cannot catch (e.g. a component that legitimately fans out new keys forever).
 *
 * Cancellation: `SimulateRunOptions.signal` (an AbortSignal) rejects the pending
 * `simulate()` promise cooperatively at each await point and interrupts an
 * in-flight `provider.resolve()`. The HARD-CANCEL path when the synchronous
 * component call itself hangs is `Worker.terminate()` from the main thread —
 * an AbortSignal cannot interrupt synchronous wasm execution.
 */

/** Discriminant identifying which limit a {@link SimulationLimitError} tripped. */
export type LimitViolation =
  | "stall"
  | "max-rounds"
  | "max-keys-per-round"
  | "max-total-entries"
  | "max-total-bytes"
  | "oversize-entry"
  | "round-timeout";

/** Typed error thrown when the rerun loop trips a configured safety limit. */
export class SimulationLimitError extends Error {
  /** Which limit was exceeded (machine-readable). */
  readonly reason: LimitViolation;
  constructor(reason: LimitViolation, message: string) {
    super(message);
    this.name = "SimulationLimitError";
    this.reason = reason;
    // Restore prototype chain for `instanceof` under transpiled targets.
    Object.setPrototypeOf(this, SimulationLimitError.prototype);
  }
}

export interface Limits {
  /** Hard cap on rerun rounds driving the component. Guarantees termination. */
  maxRounds: number;
  /** Optional cap on how many keys are resolved in a single needs-state round. */
  maxKeysPerRound?: number;
  /** Optional cap on total ledger entries accumulated across all rounds. */
  maxTotalEntries?: number;
  /** Optional cap on total accumulated present-entry XDR bytes across all rounds. */
  maxTotalBytes?: number;
  /** Optional cap on a single present entry's XDR byte length (anti-OOM guard). */
  maxBytesPerEntry?: number;
  /** Optional per-round provider `resolve()` timeout in milliseconds. */
  roundTimeoutMs?: number;
}

export const DEFAULT_LIMITS: Limits = {
  maxRounds: 16,
};

/**
 * Pure pre-check: reject a single present entry whose XDR exceeds
 * `limits.maxBytesPerEntry` before it is folded into the accumulated state.
 * Usable as a wasm-code-size / oversize-blob guard by callers holding raw XDR.
 */
export function assertEntrySize(entryXdr: Uint8Array, limits: Limits): void {
  const cap = limits.maxBytesPerEntry;
  if (cap !== undefined && entryXdr.length > cap) {
    throw new SimulationLimitError(
      "oversize-entry",
      `ledger entry XDR is ${entryXdr.length} bytes, exceeds maxBytesPerEntry (${cap})`,
    );
  }
}

/**
 * Stateful accounting the simulator loop calls each round. Centralizes every
 * limit decision so the loop body stays a straight-line description of the
 * rerun protocol. Each method throws {@link SimulationLimitError} on violation.
 */
export class LimitTracker {
  readonly limits: Limits;
  private roundCount = 0;
  private totalEntries = 0;
  private totalBytes = 0;

  constructor(limits: Limits) {
    this.limits = limits;
  }

  /** Component runs performed so far. */
  get rounds(): number {
    return this.roundCount;
  }

  /** Account for entries seeded before the loop (counts toward totals + size). */
  admitSeedEntries(entries: ReadonlyArray<{ entryXdr: Uint8Array }>): void {
    for (const e of entries) this.admitEntry(e.entryXdr);
  }

  /** Mark the start of a component run and return the 1-based round number. */
  enterRound(): number {
    this.roundCount += 1;
    return this.roundCount;
  }

  /**
   * Validate the batch of not-yet-attempted keys a needs-state round produced.
   * Empty batch => STALL (only already-attempted keys were re-requested).
   */
  assertProgress(batch: ReadonlyArray<Uint8Array>, requested: number): void {
    if (batch.length === 0) {
      throw new SimulationLimitError(
        "stall",
        `simulate stalled: needs-state requested ${requested} key(s), ` +
          `all of which were already attempted (round ${this.roundCount})`,
      );
    }
    const cap = this.limits.maxKeysPerRound;
    if (cap !== undefined && batch.length > cap) {
      throw new SimulationLimitError(
        "max-keys-per-round",
        `needs-state round requested ${batch.length} new key(s), ` +
          `exceeds maxKeysPerRound (${cap})`,
      );
    }
  }

  /** Backstop before performing another component run after a needs-state. */
  assertCanRerun(): void {
    if (this.roundCount >= this.limits.maxRounds) {
      throw new SimulationLimitError(
        "max-rounds",
        `simulate exceeded maxRounds (${this.limits.maxRounds})`,
      );
    }
  }

  /**
   * Account for one resolved present entry: enforce per-entry size, then update
   * and enforce the accumulated entry-count and byte caps.
   */
  admitEntry(entryXdr: Uint8Array): void {
    assertEntrySize(entryXdr, this.limits);
    this.totalEntries += 1;
    this.totalBytes += entryXdr.length;

    const entryCap = this.limits.maxTotalEntries;
    if (entryCap !== undefined && this.totalEntries > entryCap) {
      throw new SimulationLimitError(
        "max-total-entries",
        `accumulated ${this.totalEntries} ledger entries, exceeds maxTotalEntries (${entryCap})`,
      );
    }
    const byteCap = this.limits.maxTotalBytes;
    if (byteCap !== undefined && this.totalBytes > byteCap) {
      throw new SimulationLimitError(
        "max-total-bytes",
        `accumulated ${this.totalBytes} entry bytes, exceeds maxTotalBytes (${byteCap})`,
      );
    }
  }
}
