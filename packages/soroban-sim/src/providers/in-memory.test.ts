import { describe, expect, it } from "vitest";
import type {
  LedgerContext,
  Outcome,
  Request,
} from "../generated/soroban-sim-component.js";
import {
  type Component,
  type SimulateInput,
  SorobanSimulator,
} from "../simulator.js";
import { InMemoryStateProvider } from "./in-memory.js";

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

/** Component whose simulate() replays a scripted sequence of outcomes. */
function scriptedComponent(
  script: (request: Request, call: number) => Outcome,
): Component & { calls: Request[] } {
  const calls: Request[] = [];
  return {
    calls,
    simulate(request: Request): Outcome {
      calls.push({
        ...request,
        ledgerEntries: [...request.ledgerEntries],
        knownAbsentKeys: [...request.knownAbsentKeys],
      });
      return script(request, calls.length - 1);
    },
  };
}

describe("InMemoryStateProvider", () => {
  it("resolves present/absent 1:1 with input order", async () => {
    const kA = new Uint8Array([0x01]);
    const kB = new Uint8Array([0x02]);
    const kC = new Uint8Array([0x03]);
    const entryA = new Uint8Array([0xde, 0xad]);

    const provider = new InMemoryStateProvider();
    provider.setPresent(kA, { entryXdr: entryA, liveUntilLedger: 500 });
    provider.setAbsent(kB);
    // kC is unmapped -> default absent policy.

    const res = await provider.resolve([kC, kB, kA]);
    expect(res).toEqual([
      { status: "absent" },
      { status: "absent" },
      { status: "present", entry: { entryXdr: entryA, liveUntilLedger: 500 } },
    ]);
  });

  it("accepts a keyXdr/resolution pair iterable in the constructor", async () => {
    const kA = new Uint8Array([0x0a]);
    const entryA = new Uint8Array([0x11]);
    const provider = new InMemoryStateProvider([
      [kA, { status: "present", entry: { entryXdr: entryA } }],
    ]);
    const [res] = await provider.resolve([kA]);
    expect(res).toEqual({
      status: "present",
      entry: { entryXdr: entryA },
    });
  });

  it("throws for an unmapped key when unmappedPolicy is 'missing'", async () => {
    const provider = new InMemoryStateProvider(undefined, {
      unmappedPolicy: "missing",
    });
    await expect(provider.resolve([new Uint8Array([0x99])])).rejects.toThrow(
      /no mapping/,
    );
  });

  it("drives the full simulator rerun loop: partial -> present/absent -> complete", async () => {
    const codeKey = new Uint8Array([0x01, 0x01]);
    const dataKey = new Uint8Array([0x02, 0x02]);
    const absentKey = new Uint8Array([0x03, 0x03]);
    const codeEntry = new Uint8Array([0xc0, 0xde]);
    const dataEntry = new Uint8Array([0xda, 0x7a]);
    const done = new Uint8Array([0xff]);

    // Round 0: needs code + data + a key that is absent.
    // Round 1 (after present entries + absent key fed back): completes.
    const component = scriptedComponent((_req, call) =>
      call === 0 ? needs([codeKey, dataKey, absentKey]) : complete(done),
    );

    const provider = new InMemoryStateProvider();
    provider.setPresent(codeKey, { entryXdr: codeEntry, liveUntilLedger: 1000 });
    provider.setPresent(dataKey, { entryXdr: dataEntry, liveUntilLedger: 2000 });
    provider.setAbsent(absentKey);

    const sim = await SorobanSimulator.create({
      networkPassphrase: "Test",
      provider,
      component,
    });

    const result = await sim.simulate(INPUT);
    expect(result.resultXdr).toEqual(done);
    expect(result.rounds).toBe(2);
    expect(result.requestedKeyCount).toBe(3);

    // The rerun request carried both present entries and the proven-absent key.
    const rerun = component.calls[1]!;
    expect(rerun.ledgerEntries).toEqual([
      { entryXdr: codeEntry, liveUntilLedger: 1000 },
      { entryXdr: dataEntry, liveUntilLedger: 2000 },
    ]);
    expect(rerun.knownAbsentKeys.map(keyHex)).toEqual([keyHex(absentKey)]);
  });

  it("does not loop when a key resolves absent and is re-requested (fed as known-absent)", async () => {
    const absentKey = new Uint8Array([0x42]);
    // The component keeps asking for the same absent key; the loop must stall
    // (attempted-set guard) rather than spin forever.
    const component = scriptedComponent(() => needs([absentKey]));
    const provider = new InMemoryStateProvider().setAbsent(absentKey);
    const sim = await SorobanSimulator.create({
      networkPassphrase: "Test",
      provider,
      component,
    });
    await expect(sim.simulate(INPUT)).rejects.toThrow(/stalled/);
  });
});
