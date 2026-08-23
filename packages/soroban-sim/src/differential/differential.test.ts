/**
 * M3 RPC-equivalence differential test.
 *
 * Drives soroban-sim over committed parity fixtures captured from a live Stellar
 * testnet RPC `simulateTransaction`, then compares the two outputs field-by-field
 * (see `./compare.ts`). Two fixtures are exercised:
 *   - `native-sac-decimals`  — read-only SAC `decimals()` (result + footprint RO)
 *   - `native-sac-transfer`  — SAC `transfer()` exercising the WRITE + AUTH +
 *     contract-event surface (footprint RW, auth entry, transfer event)
 *
 * Each fixture is simulated with `applyRpcAdjustments = true`
 * (`AdjustmentSource::RpcDefault`) so the decoded `SorobanTransactionData`
 * resources/fee carry the same adjustment stellar-rpc's preflight applies.
 *
 * MUST-match fields (asserted `match`): invocation result ScVal, footprint
 * read-only/read-write sets, auth entries, contract events. No field may be an
 * outright `MISMATCH`. Resource/fee residuals are classified `divergent-explained`
 * (documented, non-zero) — never silently force-passed.
 *
 * ── Protocol / env-host version note (real findings, not papered over) ──────
 * The two `native-sac-*` fixtures were captured against Stellar testnet at ledger
 * protocol 27, while `soroban-sim-core` links `soroban-env-host` 28.0.2 (protocol
 * 28). The host's replay-safety guard (`check_ledger_protocol_supported`, from
 * `Host::set_ledger_info` during recording setup) rejects *any* ledger protocol
 * != 28 with a bare `Error(Context, InternalError)`, so the harness pins the
 * ledger context to 28 (`HOST_PROTOCOL_VERSION`).
 *
 * `local-add-i32` was captured against a LOCAL protocol-28 quickstart node, so
 * protocol matches on both sides — yet a small raw-instruction residual remains.
 * Root cause (grounded via `stellar-core version`): that quickstart core embeds
 * soroban-env-host 28.0.0, whereas we pin 28.0.2 — a patch-level CPU-metering
 * calibration difference. In all cases RESULT / FOOTPRINT / AUTH / EVENT / bytes
 * are byte-equal; only the raw instruction count (and the CPU-fee component that
 * tracks it) diverges, which the `instructions (RpcDefault)` field documents.
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
 * the file header for why the fixtures' captured protocol (27) is overridden.
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

function loadFixture(name: string): ParityFixture {
  const path = fileURLToPath(
    new URL(`../../../../fixtures/parity/${name}.json`, import.meta.url),
  );
  return JSON.parse(readFileSync(path, "utf8")) as ParityFixture;
}

/** Fixtures exercised by the parity suite, and the fields each is expected to cover. */
const FIXTURES = [
  { name: "native-sac-decimals", expectRw: false, expectAuth: false },
  { name: "native-sac-transfer", expectRw: true, expectAuth: true },
  // Captured against a LOCAL protocol-28 quickstart node (custom add-i32 wasm),
  // matching the host's pinned protocol — the residual-closing case.
  { name: "local-add-i32", expectRw: false, expectAuth: false },
] as const;

describe("RPC equivalence (RpcDefault adjustments)", () => {
  let simulator: SorobanSimulator;

  beforeAll(async () => {
    simulator = await SorobanSimulator.create({
      // networkPassphrase is per-fixture but identical across our testnet
      // captures; set from the first fixture. No provider: each fixture snapshot
      // is complete, so the simulation resolves in a single round.
      networkPassphrase: loadFixture(FIXTURES[0].name).networkPassphrase,
    });
  });

  for (const spec of FIXTURES) {
    it(`matches recorded RPC simulateTransaction: ${spec.name}`, async () => {
      const fixture = loadFixture(spec.name);
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
        // Reproduce stellar-rpc preflight's resource/fee adjustment so the
        // decoded transaction_data resources/fee are comparable to the fixture.
        applyRpcAdjustments: true,
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
          `(fixture captured at ${ctx.protocolVersion}), RpcDefault adjustments\n\n` +
          formatParityTable(report) +
          "\n",
      );

      const verdictOf = (field: string) =>
        report.fields.find((f) => f.field === field)?.verdict;

      // MUST-match fields (all fixtures).
      expect(verdictOf("result (ScVal)")).toBe("match");
      expect(verdictOf("footprint readOnly")).toBe("match");
      expect(verdictOf("footprint readWrite")).toBe("match");
      expect(verdictOf("auth entries")).toBe("match");
      expect(verdictOf("contract events")).toBe("match");

      // Read/write bytes match under RpcDefault (no byte-count adjustment).
      expect(verdictOf("read bytes")).toBe("match");
      expect(verdictOf("write bytes")).toBe("match");

      // Fixture-specific coverage assertions: the transfer case must actually
      // exercise the write footprint + auth surface (guards against a fixture
      // regressing to a read-only shape).
      const rwField = report.fields.find((f) => f.field === "footprint readWrite");
      const authField = report.fields.find((f) => f.field === "auth entries");
      const eventsField = report.fields.find((f) => f.field === "contract events");
      if (spec.expectRw) {
        expect(rwField?.rpc).toBe("2 key(s)");
        expect(rwField?.local).toBe("2 key(s)");
      }
      if (spec.expectAuth) {
        expect(authField?.local).toBe("1");
        expect(authField?.rpc).toBe("1");
        expect(eventsField?.local).toBe("1");
        expect(eventsField?.rpc).toBe("1");
      }

      // No field may be an outright MISMATCH; divergent-explained is tolerated.
      const mismatches = report.fields.filter((f) => f.verdict === "MISMATCH");
      expect(
        mismatches,
        `unexpected MISMATCH field(s): ${mismatches
          .map((f) => `${f.field} (local=${f.local}, rpc=${f.rpc})`)
          .join("; ")}`,
      ).toEqual([]);
    });
  }
});
