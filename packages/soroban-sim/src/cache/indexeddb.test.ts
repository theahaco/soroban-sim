import "fake-indexeddb/auto";
import { beforeEach, describe, expect, it } from "vitest";
import type {
  LedgerContext,
  Outcome,
  Request,
} from "../generated/soroban-sim-component.js";
import { InMemoryStateProvider } from "../providers/in-memory.js";
import {
  type Component,
  type SimulateInput,
  SorobanSimulator,
} from "../simulator.js";
import type { EntryResolution, StateProvider } from "../state-provider.js";
import { CachingStateProvider } from "./caching-provider.js";
import { IndexedDbEntryCache } from "./indexeddb.js";

const NETWORK_A = new Uint8Array(32).fill(0xa1);
const NETWORK_B = new Uint8Array(32).fill(0xb2);

const CTX: LedgerContext = {
  protocolVersion: 22,
  sequenceNumber: 100,
  timestamp: 0n,
  networkId: new Uint8Array(32),
  baseReserve: 0,
  minTempEntryTtl: 16,
  minPersistentEntryTtl: 120960,
  maxEntryTtl: 3110400,
};

/** Counts how many keys the inner provider was actually asked to resolve. */
class CountingProvider implements StateProvider {
  calls = 0;
  resolvedKeyCount = 0;
  constructor(private readonly inner: StateProvider) {}
  async resolve(keyXdrs: Uint8Array[]): Promise<EntryResolution[]> {
    this.calls += 1;
    this.resolvedKeyCount += keyXdrs.length;
    return this.inner.resolve(keyXdrs);
  }
}

let dbCounter = 0;
/** A fresh, isolated cache DB per test to avoid cross-test contamination. */
async function freshCache(): Promise<IndexedDbEntryCache> {
  dbCounter += 1;
  return IndexedDbEntryCache.open({ dbName: `test-cache-${dbCounter}` });
}

describe("IndexedDbEntryCache + CachingStateProvider", () => {
  let cache: IndexedDbEntryCache;

  beforeEach(async () => {
    cache = await freshCache();
  });

  it("miss then hit: delegates once, then serves the present entry from cache", async () => {
    const key = new Uint8Array([0x01, 0x02, 0x03]);
    const entryXdr = new Uint8Array([0xde, 0xad, 0xbe, 0xef]);

    const inner = new CountingProvider(
      new InMemoryStateProvider([
        [key, { status: "present", entry: { entryXdr, liveUntilLedger: 4242 } }],
      ]),
    );
    const provider = new CachingStateProvider({
      inner,
      db: cache,
      context: { networkId: NETWORK_A, ledgerSeq: 100 },
    });

    const first = await provider.resolve([key]);
    expect(first).toEqual([
      { status: "present", entry: { entryXdr, liveUntilLedger: 4242 } },
    ]);
    expect(inner.resolvedKeyCount).toBe(1);

    const second = await provider.resolve([key]);
    expect(second).toEqual([
      { status: "present", entry: { entryXdr, liveUntilLedger: 4242 } },
    ]);
    // Inner provider was NOT consulted the second time.
    expect(inner.resolvedKeyCount).toBe(1);
    expect(await cache.count()).toBe(1);
  });

  it("preserves TTL verbatim (no synthesized liveUntilLedger)", async () => {
    const key = new Uint8Array([0x11]);
    const entryXdr = new Uint8Array([0x22]);
    // Present entry with NO liveUntilLedger must round-trip without one.
    const inner = new InMemoryStateProvider([
      [key, { status: "present", entry: { entryXdr } }],
    ]);
    const provider = new CachingStateProvider({
      inner,
      db: cache,
      context: { networkId: NETWORK_A, ledgerSeq: 7 },
    });

    await provider.resolve([key]);
    const [hit] = await provider.resolve([key]);
    expect(hit).toEqual({ status: "present", entry: { entryXdr } });
    expect(hit && hit.status === "present" && "liveUntilLedger" in hit.entry).toBe(
      false,
    );
  });

  it("absent-cache: a proven-absent key is cached and never refetched", async () => {
    const key = new Uint8Array([0x99]);
    // Default InMemoryStateProvider policy resolves an unmapped key as absent.
    const inner = new CountingProvider(new InMemoryStateProvider());
    const provider = new CachingStateProvider({
      inner,
      db: cache,
      context: { networkId: NETWORK_A, ledgerSeq: 100 },
    });

    const first = await provider.resolve([key]);
    expect(first).toEqual([{ status: "absent" }]);
    expect(inner.calls).toBe(1);

    const second = await provider.resolve([key]);
    expect(second).toEqual([{ status: "absent" }]);
    // Absent marker served from cache — inner untouched.
    expect(inner.calls).toBe(1);
  });

  it("cached ABSENT stops the rerun loop from refetching across simulate() runs", async () => {
    const absentKey = new Uint8Array([0x42, 0x42]);
    const done = new Uint8Array([0xff]);

    // Component asks for `absentKey` until it appears in knownAbsentKeys, then
    // completes. This mirrors the real needs-state protocol per simulate() run.
    const component: Component = {
      simulate(request: Request): Outcome {
        const known = request.knownAbsentKeys.some(
          (k) => k.length === absentKey.length && k[0] === absentKey[0],
        );
        return known
          ? { tag: "complete", val: done }
          : { tag: "needs-state", val: [absentKey] };
      },
    };

    const inner = new CountingProvider(new InMemoryStateProvider());
    const caching = new CachingStateProvider({
      inner,
      db: cache,
      context: { networkId: NETWORK_A, ledgerSeq: 100 },
    });

    const input: SimulateInput = {
      hostFunctionXdr: new Uint8Array([0xaa]),
      sourceAccountXdr: new Uint8Array([0xbb]),
      ledgerContext: CTX,
    };

    const sim = await SorobanSimulator.create({
      networkPassphrase: "Test",
      provider: caching,
      component,
    });

    // Two independent runs: fresh attempted-set each time. The absent key is
    // resolved from the network only in the FIRST run; the SECOND is a cache hit.
    const r1 = await sim.simulate(input);
    expect(r1.resultXdr).toEqual(done);
    expect(inner.calls).toBe(1);

    const r2 = await sim.simulate(input);
    expect(r2.resultXdr).toEqual(done);
    expect(inner.calls).toBe(1); // no refetch of the absent key
  });

  it("cache identity prevents cross-network / cross-ledger contamination", async () => {
    const key = new Uint8Array([0x0a, 0x0b]);
    const entryXdr = new Uint8Array([0xca, 0xfe]);

    const inner = new CountingProvider(
      new InMemoryStateProvider([
        [key, { status: "present", entry: { entryXdr, liveUntilLedger: 900 } }],
      ]),
    );

    const providerA = new CachingStateProvider({
      inner,
      db: cache,
      context: { networkId: NETWORK_A, ledgerSeq: 100 },
    });
    // Same key, DIFFERENT ledger sequence -> different composite id.
    const providerLater = new CachingStateProvider({
      inner,
      db: cache,
      context: { networkId: NETWORK_A, ledgerSeq: 200 },
    });
    // Same key + ledger, DIFFERENT network -> different composite id.
    const providerB = new CachingStateProvider({
      inner,
      db: cache,
      context: { networkId: NETWORK_B, ledgerSeq: 100 },
    });

    await providerA.resolve([key]);
    expect(inner.resolvedKeyCount).toBe(1);

    // Different ledger: miss, so the inner provider is consulted again.
    await providerLater.resolve([key]);
    expect(inner.resolvedKeyCount).toBe(2);

    // Different network: miss again.
    await providerB.resolve([key]);
    expect(inner.resolvedKeyCount).toBe(3);

    // Re-resolving under context A now hits the cache (no new inner call).
    await providerA.resolve([key]);
    expect(inner.resolvedKeyCount).toBe(3);

    // Three distinct ids were persisted for the same key XDR.
    expect(await cache.count()).toBe(3);
  });

  it("optional txHash further scopes the cache id", async () => {
    const key = new Uint8Array([0x77]);
    const entryXdr = new Uint8Array([0x88]);
    const inner = new CountingProvider(
      new InMemoryStateProvider([
        [key, { status: "present", entry: { entryXdr } }],
      ]),
    );
    const withTx = new CachingStateProvider({
      inner,
      db: cache,
      context: { networkId: NETWORK_A, ledgerSeq: 5, txHash: "tx-1" },
    });
    const withoutTx = new CachingStateProvider({
      inner,
      db: cache,
      context: { networkId: NETWORK_A, ledgerSeq: 5 },
    });

    await withTx.resolve([key]);
    await withoutTx.resolve([key]);
    // Distinct ids -> both consulted the inner provider.
    expect(inner.resolvedKeyCount).toBe(2);
    expect(await cache.count()).toBe(2);
  });

  it("versions the physical database name with the schema version", async () => {
    expect(cache.dbName).toMatch(/\/v\d+$/);
    expect(cache.schemaVersion).toBeGreaterThanOrEqual(1);
  });

  it("resolve([]) short-circuits without touching the inner provider", async () => {
    const inner = new CountingProvider(new InMemoryStateProvider());
    const provider = new CachingStateProvider({
      inner,
      db: cache,
      context: { networkId: NETWORK_A, ledgerSeq: 1 },
    });
    expect(await provider.resolve([])).toEqual([]);
    expect(inner.calls).toBe(0);
  });
});
