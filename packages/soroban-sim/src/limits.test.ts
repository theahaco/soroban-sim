import { describe, expect, it } from "vitest";
import type {
  LedgerContext,
  Outcome,
  Request,
} from "./generated/soroban-sim-component.js";
import type { EntryResolution, StateProvider } from "./state-provider.js";
import {
  assertEntrySize,
  DEFAULT_LIMITS,
  type Limits,
  LimitTracker,
  SimulationLimitError,
} from "./limits.js";
import {
  type Component,
  type SimulateInput,
  SorobanSimulator,
} from "./simulator.js";

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

const INPUT: SimulateInput = {
  hostFunctionXdr: new Uint8Array([0xaa]),
  sourceAccountXdr: new Uint8Array([0xbb]),
  ledgerContext: CTX,
};

const keyHex = (bytes: Uint8Array) =>
  [...bytes].map((b) => b.toString(16).padStart(2, "0")).join("");

const complete = (val: Uint8Array): Outcome => ({ tag: "complete", val });
const needs = (val: Uint8Array[]): Outcome => ({ tag: "needs-state", val });

function inMemoryProvider(
  table: Record<string, EntryResolution>,
): StateProvider {
  return {
    resolve: (keys) =>
      Promise.resolve(keys.map((k) => table[keyHex(k)] ?? { status: "absent" })),
  };
}

function scriptedComponent(
  script: (request: Request, call: number) => Outcome,
): Component {
  let call = 0;
  return {
    simulate: (request: Request): Outcome => script(request, call++),
  };
}

// --- Pure helper / tracker unit tests -------------------------------------

describe("limits helpers", () => {
  it("DEFAULT_LIMITS caps rounds at 16", () => {
    expect(DEFAULT_LIMITS.maxRounds).toBe(16);
  });

  it("assertEntrySize rejects an oversize entry with a typed error", () => {
    const limits: Limits = { maxRounds: 16, maxBytesPerEntry: 4 };
    expect(() => assertEntrySize(new Uint8Array(3), limits)).not.toThrow();
    try {
      assertEntrySize(new Uint8Array(5), limits);
      throw new Error("expected throw");
    } catch (e) {
      expect(e).toBeInstanceOf(SimulationLimitError);
      expect((e as SimulationLimitError).reason).toBe("oversize-entry");
    }
  });

  it("LimitTracker.assertProgress flags a stall on an empty batch", () => {
    const t = new LimitTracker({ maxRounds: 16 });
    t.enterRound();
    expect(() => t.assertProgress([], 2)).toThrow(SimulationLimitError);
    try {
      t.assertProgress([], 2);
    } catch (e) {
      expect((e as SimulationLimitError).reason).toBe("stall");
    }
  });

  it("LimitTracker.assertProgress enforces maxKeysPerRound", () => {
    const t = new LimitTracker({ maxRounds: 16, maxKeysPerRound: 1 });
    t.enterRound();
    try {
      t.assertProgress([new Uint8Array([1]), new Uint8Array([2])], 2);
      throw new Error("expected throw");
    } catch (e) {
      expect((e as SimulationLimitError).reason).toBe("max-keys-per-round");
    }
  });

  it("LimitTracker.admitEntry enforces total entry and byte caps", () => {
    const byCount = new LimitTracker({ maxRounds: 16, maxTotalEntries: 1 });
    byCount.admitEntry(new Uint8Array(1));
    try {
      byCount.admitEntry(new Uint8Array(1));
      throw new Error("expected throw");
    } catch (e) {
      expect((e as SimulationLimitError).reason).toBe("max-total-entries");
    }

    const byBytes = new LimitTracker({ maxRounds: 16, maxTotalBytes: 3 });
    byBytes.admitEntry(new Uint8Array(2));
    try {
      byBytes.admitEntry(new Uint8Array(2));
      throw new Error("expected throw");
    } catch (e) {
      expect((e as SimulationLimitError).reason).toBe("max-total-bytes");
    }
  });
});

// --- Loop-integration tests ------------------------------------------------

describe("SorobanSimulator limit enforcement", () => {
  async function sim(
    component: Component,
    provider: StateProvider | undefined,
    limits?: Partial<Limits>,
  ) {
    return SorobanSimulator.create({
      networkPassphrase: "Test",
      component,
      provider,
      limits,
    });
  }

  it("stall: all requested keys resolve absent, stops instead of looping", async () => {
    const keyA = new Uint8Array([0x07]);
    const component = scriptedComponent(() => needs([keyA])); // always re-asks keyA
    const provider = inMemoryProvider({ [keyHex(keyA)]: { status: "absent" } });
    const s = await sim(component, provider);
    await expect(s.simulate(INPUT)).rejects.toMatchObject({
      name: "SimulationLimitError",
      reason: "stall",
    });
  });

  it("maxRounds: a fresh key every round trips the cap", async () => {
    let n = 0;
    const component = scriptedComponent(() => needs([new Uint8Array([++n])]));
    const provider = inMemoryProvider({}); // everything absent -> a new key each round
    const s = await sim(component, provider, { maxRounds: 3 });
    const err = await s.simulate(INPUT).catch((e) => e);
    expect(err).toBeInstanceOf(SimulationLimitError);
    expect(err.reason).toBe("max-rounds");
  });

  it("oversize entry: a present entry over maxBytesPerEntry is rejected", async () => {
    const keyA = new Uint8Array([0x01]);
    const component = scriptedComponent((_r, call) =>
      call === 0 ? needs([keyA]) : complete(new Uint8Array([0x99])),
    );
    const provider = inMemoryProvider({
      [keyHex(keyA)]: {
        status: "present",
        entry: { entryXdr: new Uint8Array(64), liveUntilLedger: 500 },
      },
    });
    const s = await sim(component, provider, { maxBytesPerEntry: 16 });
    await expect(s.simulate(INPUT)).rejects.toMatchObject({
      name: "SimulationLimitError",
      reason: "oversize-entry",
    });
  });

  it("maxTotalBytes: accumulated present entries over the cap are rejected", async () => {
    const keyA = new Uint8Array([0x01]);
    const keyB = new Uint8Array([0x02]);
    const component = scriptedComponent((_r, call) =>
      call === 0 ? needs([keyA, keyB]) : complete(new Uint8Array([0x00])),
    );
    const provider = inMemoryProvider({
      [keyHex(keyA)]: {
        status: "present",
        entry: { entryXdr: new Uint8Array(8), liveUntilLedger: 1 },
      },
      [keyHex(keyB)]: {
        status: "present",
        entry: { entryXdr: new Uint8Array(8), liveUntilLedger: 1 },
      },
    });
    const s = await sim(component, provider, { maxTotalBytes: 10 });
    await expect(s.simulate(INPUT)).rejects.toMatchObject({
      reason: "max-total-bytes",
    });
  });

  it("AbortSignal: cancellation rejects the pending simulate", async () => {
    const controller = new AbortController();
    const component = scriptedComponent(() => {
      controller.abort();
      return needs([new Uint8Array([1])]);
    });
    const provider = inMemoryProvider({});
    const s = await sim(component, provider);
    await expect(
      s.simulate(INPUT, { signal: controller.signal }),
    ).rejects.toMatchObject({ name: "AbortError" });
  });

  it("AbortSignal: interrupts an in-flight provider.resolve", async () => {
    const controller = new AbortController();
    const keyA = new Uint8Array([0x01]);
    const component = scriptedComponent(() => needs([keyA]));
    // Provider that never resolves; only the abort can settle the loop.
    const provider: StateProvider = {
      resolve: () => new Promise<EntryResolution[]>(() => {}),
    };
    const s = await sim(component, provider);
    const p = s.simulate(INPUT, { signal: controller.signal });
    controller.abort();
    await expect(p).rejects.toMatchObject({ name: "AbortError" });
  });

  it("roundTimeoutMs: a slow provider trips a round-timeout", async () => {
    const keyA = new Uint8Array([0x01]);
    const component = scriptedComponent(() => needs([keyA]));
    const provider: StateProvider = {
      resolve: () =>
        new Promise<EntryResolution[]>((resolve) =>
          setTimeout(() => resolve([{ status: "absent" }]), 50),
        ),
    };
    const s = await sim(component, provider, { roundTimeoutMs: 5 });
    await expect(s.simulate(INPUT)).rejects.toMatchObject({
      name: "SimulationLimitError",
      reason: "round-timeout",
    });
  });
});
