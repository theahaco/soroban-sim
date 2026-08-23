/**
 * M3 RPC-equivalence differential test.
 *
 * Drives soroban-sim over a committed parity fixture captured from a live
 * Stellar testnet RPC `simulateTransaction`, then compares the two outputs
 * field-by-field (see `./compare.ts`). The MUST-match fields (invocation result
 * ScVal, footprint read-only/read-write sets, auth, contract events) are
 * asserted equal; resource/fee divergence is tolerated as `divergent-expected`.
 *
 * ── Protocol version note (a real finding, not papered over) ────────────────
 * The fixture was captured against Stellar testnet at ledger protocol 27, but
 * `soroban-sim-core` links `soroban-env-host` 28.0.2, whose network interface
 * protocol is 28. The host's replay-safety guard (`check_ledger_protocol_supported`,
 * invoked from `Host::set_ledger_info` during recording setup) rejects *any*
 * ledger protocol != 28 with a bare `Error(Context, InternalError)` — no
 * diagnostics, thrown before the contract body runs. Feeding the fixture's
 * protocol 27 therefore fails with that internal error. Since the SAC `decimals()`
 * call is protocol-invariant, the harness pins the ledger context to the host's
 * supported protocol (28) when driving the local simulation; RESULT and FOOTPRINT
 * parity are unaffected. This is documented as HOST_PROTOCOL_VERSION below.
 */
import { readFileSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { beforeAll, describe, expect, it } from "vitest";

// IMPORTANT: install the file:// fetch polyfill before anything dynamically
// imports the jco-generated component. The generated module self-instantiates at
// import time via `WebAssembly.compileStreaming(fetch(new URL(...core.wasm)))`;
// Node's fetch cannot read file: URLs, so we shim it here. `SorobanSimulator` is
// imported below as a *value* but only lazily imports the component inside
// `create()`, so this shim is in place before the wasm is ever fetched.
const originalFetch: typeof fetch = globalThis.fetch;
globalThis.fetch = (async (
  input: RequestInfo | URL,
  init?: RequestInit,
): Promise<Response> => {
  const url =
    typeof input === "string"
      ? input
      : input instanceof URL
        ? input.href
        : input.url;
  if (url.startsWith("file:")) {
    const buf = await readFile(fileURLToPath(url));
    // Copy into a fresh ArrayBuffer so the body is an unambiguous BufferSource
    // (TS 5.9 types plain Uint8Array over ArrayBufferLike, not ArrayBuffer).
    const body = new ArrayBuffer(buf.byteLength);
    new Uint8Array(body).set(buf);
    return new Response(body, {
      headers: { "content-type": "application/wasm" },
    });
  }
  return originalFetch(input, init);
}) as typeof fetch;

import type {
  LedgerContext,
  LedgerEntry,
} from "../generated/soroban-sim-component.js";
import { SorobanSimulator } from "../simulator.js";
import {
  buildParityReport,
  formatParityTable,
  type RpcFixtureData,
} from "./compare.js";
import { decodeSsim } from "./decode-ssim.js";

/**
 * Ledger protocol version the linked `soroban-env-host` (28.0.2) will run. See
 * the file header for why the fixture's captured protocol (27) is overridden.
 */
const HOST_PROTOCOL_VERSION = 28;

interface ParityFixture {
  networkPassphrase: string;
  case: string;
  functionName: string;
  txXdr: string;
  ledgerContext: {
    protocolVersion: number;
    sequenceNumber: number;
    timestamp: number;
    networkId: string;
    baseReserve: number;
    minTempEntryTtl: number;
    minPersistentEntryTtl: number;
    maxEntryTtl: number;
  };
  snapshot: { keyXdr: string; entryXdr: string; liveUntilLedger: number | null }[];
  rpc: RpcFixtureData;
}

function b64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i += 1) out[i] = bin.charCodeAt(i);
  return out;
}

function hexToBytes(hex: string): Uint8Array {
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i += 1) {
    out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  }
  return out;
}

function loadFixture(): ParityFixture {
  const path = fileURLToPath(
    new URL(
      "../../../../fixtures/parity/native-sac-decimals.json",
      import.meta.url,
    ),
  );
  return JSON.parse(readFileSync(path, "utf8")) as ParityFixture;
}

describe("RPC equivalence: native SAC decimals()", () => {
  const fixture = loadFixture();
  let simulator: SorobanSimulator;

  beforeAll(async () => {
    simulator = await SorobanSimulator.create({
      networkPassphrase: fixture.networkPassphrase,
      // No provider: the fixture snapshot is complete, so the simulation
      // resolves to `complete` in a single round with no state fetches.
    });
  });

  it("matches the recorded RPC simulateTransaction on the MUST-match fields", async () => {
    const ctx = fixture.ledgerContext;
    const ledgerContext: LedgerContext = {
      protocolVersion: HOST_PROTOCOL_VERSION, // see file header: 27 -> 28
      sequenceNumber: ctx.sequenceNumber,
      timestamp: BigInt(ctx.timestamp),
      networkId: hexToBytes(ctx.networkId),
      baseReserve: ctx.baseReserve,
      minTempEntryTtl: ctx.minTempEntryTtl,
      minPersistentEntryTtl: ctx.minPersistentEntryTtl,
      maxEntryTtl: ctx.maxEntryTtl,
    };

    const seedEntries: LedgerEntry[] = fixture.snapshot.map((e) => ({
      entryXdr: b64ToBytes(e.entryXdr),
      liveUntilLedger: e.liveUntilLedger ?? undefined,
    }));

    const result = await simulator.simulateTransaction({
      transactionXdr: b64ToBytes(fixture.txXdr),
      ledgerContext,
      seedEntries,
      // The component requires a 32-byte PRNG seed.
      basePrngSeed: new Uint8Array(32),
      enableDiagnostics: true,
    });

    // Full snapshot seeded => resolves without any needs-state round trips.
    expect(result.rounds).toBe(1);
    expect(result.requestedKeyCount).toBe(0);

    const decoded = decodeSsim(result.resultXdr);

    // The local run must actually succeed (not an inline host/contract error).
    // A failure here is a real finding (e.g. the protocol-version guard) — fail
    // loudly with the host error rather than papering over it.
    if (!decoded.result.ok) {
      throw new Error(
        `local simulation returned an inline error instead of a value: ${decoded.result.error}`,
      );
    }
    expect(decoded.result.ok).toBe(true);

    const report = buildParityReport(decoded, fixture.rpc);

    // eslint-disable-next-line no-console
    console.log(
      `\nParity report — ${fixture.case} (${fixture.functionName}):\n` +
        `local ledger protocol pinned to ${HOST_PROTOCOL_VERSION} ` +
        `(fixture captured at ${ctx.protocolVersion})\n\n` +
        formatParityTable(report) +
        "\n",
    );

    const verdictOf = (field: string) =>
      report.fields.find((f) => f.field === field)?.verdict;

    // MUST-match fields.
    expect(verdictOf("result (ScVal)")).toBe("match");
    expect(verdictOf("footprint readOnly")).toBe("match");
    expect(verdictOf("footprint readWrite")).toBe("match");

    // No field may be an outright MISMATCH; divergent-expected is tolerated.
    const mismatches = report.fields.filter((f) => f.verdict === "MISMATCH");
    expect(
      mismatches,
      `unexpected MISMATCH field(s): ${mismatches
        .map((f) => `${f.field} (local=${f.local}, rpc=${f.rpc})`)
        .join("; ")}`,
    ).toEqual([]);
  });
});
