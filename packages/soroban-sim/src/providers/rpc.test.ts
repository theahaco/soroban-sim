import { rpc, xdr } from "@stellar/stellar-sdk";
import { describe, expect, it, vi } from "vitest";
import {
  type LedgerEntriesServer,
  RpcStateProvider,
} from "./rpc.js";

/** Build a distinct TTL LedgerKey (simple, self-contained XDR). */
function ttlKey(seed: number): xdr.LedgerKey {
  const hash = new Uint8Array(32);
  hash[0] = seed;
  return xdr.LedgerKey.ttl(new xdr.LedgerKeyTtl({ keyHash: hash }));
}

/** Build a TtlEntry LedgerEntryData for a given key seed + TTL. */
function ttlData(seed: number, liveUntilLedgerSeq: number): xdr.LedgerEntryData {
  const hash = new Uint8Array(32);
  hash[0] = seed;
  return xdr.LedgerEntryData.ttl(
    new xdr.TtlEntry({ keyHash: hash, liveUntilLedgerSeq }),
  );
}

function ledgerEntryResult(
  key: xdr.LedgerKey,
  val: xdr.LedgerEntryData,
  opts: { lastModifiedLedgerSeq?: number; liveUntilLedgerSeq?: number } = {},
): rpc.Api.LedgerEntryResult {
  return {
    key,
    val,
    lastModifiedLedgerSeq: opts.lastModifiedLedgerSeq,
    liveUntilLedgerSeq: opts.liveUntilLedgerSeq,
  };
}

const CONFIG = {
  rpcUrl: "https://rpc.example.test",
  networkPassphrase: "Test SDF Network ; September 2015",
};

describe("RpcStateProvider", () => {
  it("maps found -> present (with TTL), not-found -> absent, in 1:1 input order", async () => {
    const k1 = ttlKey(1);
    const k2 = ttlKey(2); // absent (RPC omits it)
    const k3 = ttlKey(3);

    const d1 = ttlData(1, 111);
    const d3 = ttlData(3, 333);

    // Return entries out of input order and omit k2 to exercise re-matching.
    const getLedgerEntries = vi.fn(
      (...keys: xdr.LedgerKey[]): Promise<rpc.Api.GetLedgerEntriesResponse> => {
        expect(keys.map((k) => k.toXdr("base64"))).toEqual([
          k1.toXdr("base64"),
          k2.toXdr("base64"),
          k3.toXdr("base64"),
        ]);
        return Promise.resolve({
          latestLedger: 9000,
          entries: [
            ledgerEntryResult(k3, d3, {
              lastModifiedLedgerSeq: 30,
              liveUntilLedgerSeq: 333,
            }),
            ledgerEntryResult(k1, d1, {
              lastModifiedLedgerSeq: 10,
              liveUntilLedgerSeq: 111,
            }),
          ],
        });
      },
    );

    const server: LedgerEntriesServer = { getLedgerEntries };
    const provider = new RpcStateProvider(CONFIG, server);

    const res = await provider.resolve([
      k1.toXdr(),
      k2.toXdr(),
      k3.toXdr(),
    ]);

    expect(res).toHaveLength(3);

    // k1 -> present, TTL 111
    expect(res[0]!.status).toBe("present");
    if (res[0]!.status !== "present") throw new Error("unreachable");
    expect(res[0]!.entry.liveUntilLedger).toBe(111);

    // k2 -> absent
    expect(res[1]).toEqual({ status: "absent" });

    // k3 -> present, TTL 333
    expect(res[2]!.status).toBe("present");
    if (res[2]!.status !== "present") throw new Error("unreachable");
    expect(res[2]!.entry.liveUntilLedger).toBe(333);

    expect(getLedgerEntries).toHaveBeenCalledTimes(1);
  });

  it("wraps the RPC LedgerEntryData into a full LedgerEntry (entryXdr round-trips)", async () => {
    const k1 = ttlKey(7);
    const d1 = ttlData(7, 777);

    const server: LedgerEntriesServer = {
      getLedgerEntries: () =>
        Promise.resolve({
          latestLedger: 1,
          entries: [
            ledgerEntryResult(k1, d1, {
              lastModifiedLedgerSeq: 55,
              liveUntilLedgerSeq: 777,
            }),
          ],
        }),
    };
    const provider = new RpcStateProvider(CONFIG, server);

    const [res] = await provider.resolve([k1.toXdr()]);
    expect(res!.status).toBe("present");
    if (res!.status !== "present") throw new Error("unreachable");

    // The emitted entryXdr must decode as a full LedgerEntry whose inner data
    // equals the RPC-returned LedgerEntryData, with lastModifiedLedgerSeq kept.
    const decoded = xdr.LedgerEntry.fromXdr(res!.entry.entryXdr);
    expect(decoded.lastModifiedLedgerSeq).toBe(55);
    expect(decoded.data.toXdr("base64")).toBe(d1.toXdr("base64"));
    expect(res!.entry.liveUntilLedger).toBe(777);
  });

  it("returns [] and does not call the server for an empty batch", async () => {
    const getLedgerEntries = vi.fn();
    const provider = new RpcStateProvider(CONFIG, { getLedgerEntries });
    const res = await provider.resolve([]);
    expect(res).toEqual([]);
    expect(getLedgerEntries).not.toHaveBeenCalled();
  });

  it("marks everything absent when the RPC returns no entries", async () => {
    const k1 = ttlKey(1);
    const k2 = ttlKey(2);
    const server: LedgerEntriesServer = {
      getLedgerEntries: () =>
        Promise.resolve({ latestLedger: 1, entries: [] }),
    };
    const provider = new RpcStateProvider(CONFIG, server);
    const res = await provider.resolve([k1.toXdr(), k2.toXdr()]);
    expect(res).toEqual([{ status: "absent" }, { status: "absent" }]);
  });
});
