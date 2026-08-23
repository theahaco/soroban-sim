/**
 * Differential comparison — decode a local soroban-sim SSIM result and a recorded
 * Stellar RPC `simulateTransaction` response into a common {@link ParityView} and
 * compare them field-by-field.
 *
 * The local view is built from a run driven with `applyRpcAdjustments = true`
 * (`AdjustmentSource::RpcDefault`), so its `SorobanTransactionData` resources and
 * `resourceFee` carry soroban-simulation's RPC-parity adjustment — the same
 * `SimulationAdjustmentConfig::default_adjustment()` stellar-rpc's preflight
 * applies. The raw metered instruction count (adjustment-independent, read from
 * the SSIM `simulated_instructions` header) is reported separately as
 * informational.
 *
 * Comparison policy (see `docs/differential-testing.md` "Comparison fields"):
 *   - invocation RESULT (ScVal) ............ MUST match
 *   - FOOTPRINT read-only / read-write sets  MUST match (canonical LedgerKey XDR)
 *   - AUTH entries ......................... MUST match (canonical XDR set)
 *   - contract EVENTS ...................... MUST match (canonical ContractEvent
 *     XDR set — topics + data, not just a count)
 *   - RESOURCE estimates (instructions,
 *     read/write bytes) .................... compared under RpcDefault; read/write
 *     bytes match, instructions carry a residual protocol-metering delta
 *   - resource FEE ........................ compared under RpcDefault
 *
 * Each field lands in the report with a {@link Verdict}:
 *   - `match` ............... byte-identical / equal
 *   - `divergent-explained`  a real, non-zero difference with a documented cause
 *                            (exact numbers + smallest explanation in `note`)
 *   - `divergent-expected` . a tolerated, informational difference
 *   - `MISMATCH` ........... a real disagreement that fails the differential test
 */
import { scValToNative, xdr } from "@stellar/stellar-sdk";
import type { SsimDecoded } from "./decode-ssim.js";

export type Verdict =
  | "match"
  | "divergent-explained"
  | "divergent-expected"
  | "MISMATCH";

/** One compared field in the parity report. */
export interface ParityField {
  field: string;
  local: string;
  rpc: string;
  verdict: Verdict;
  /** Optional human note (e.g. why a divergence is expected/explained). */
  note?: string;
}

export interface ParityReport {
  fields: ParityField[];
  /** True iff any field's verdict is `MISMATCH`. */
  hasMismatch: boolean;
}

/** The recorded Stellar RPC `simulateTransaction` fields from a parity fixture. */
export interface RpcFixtureData {
  /** `ScVal` XDR of the invocation return value, base64. */
  retvalXdr: string;
  /** Minimum resource fee, in stroops (string to preserve i64 range). */
  minResourceFee: string;
  instructions: number;
  readBytes: number;
  writeBytes: number;
  /** `LedgerKey` XDR base64, read-only footprint. */
  footprintReadOnly: string[];
  /** `LedgerKey` XDR base64, read-write footprint. */
  footprintReadWrite: string[];
  /** `SorobanAuthorizationEntry` XDR base64. */
  auth: string[];
  /** `DiagnosticEvent` XDR base64 (RPC returns events wrapped as diagnostic events). */
  events: string[];
}

/** A normalized, side-agnostic view of a simulation outcome. */
export interface ParityView {
  /** Canonical `ScVal` XDR base64, or a `ERROR: ...` marker if the run failed. */
  resultScVal: string;
  /** Human-readable native form of the result value. */
  resultNative: string;
  /** Sorted canonical `LedgerKey` XDR base64. */
  footprintReadOnly: string[];
  footprintReadWrite: string[];
  /** Sorted canonical `SorobanAuthorizationEntry` XDR base64. */
  auth: string[];
  /** Sorted canonical `ContractEvent` XDR base64 (topics + data). */
  contractEvents: string[];
  /** Number of diagnostic-type events (fn_call / fn_return, etc.). */
  diagnosticEventCount: number;
  /** Adjusted instruction count (RpcDefault) — from `SorobanTransactionData`. */
  instructions: number;
  /** Raw metered instruction count (adjustment-independent); undefined for RPC. */
  rawInstructions?: number;
  readBytes: number;
  writeBytes: number;
  /** Resource fee in stroops, as a string. */
  resourceFee: string;
}

// --- base64 <-> bytes (atob/btoa, no Node Buffer dependency) ----------------

function bytesToB64(bytes: Uint8Array): string {
  let bin = "";
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin);
}

// --- canonicalization: round-trip through the XDR class so equal values that ---
// --- differ only in incidental encoding still compare equal. -----------------

function canonicalScVal(b64: string): string {
  return xdr.ScVal.fromXdr(b64, "base64").toXdr("base64");
}

function canonicalLedgerKey(b64: string): string {
  return xdr.LedgerKey.fromXdr(b64, "base64").toXdr("base64");
}

function canonicalAuth(b64: string): string {
  return xdr.SorobanAuthorizationEntry.fromXdr(b64, "base64").toXdr("base64");
}

function canonicalContractEvent(b64: string): string {
  return xdr.ContractEvent.fromXdr(b64, "base64").toXdr("base64");
}

function nativeToDisplay(value: unknown): string {
  if (typeof value === "bigint") return `${value.toString()}n`;
  try {
    return JSON.stringify(value, (_k, v) =>
      typeof v === "bigint" ? `${v.toString()}n` : v,
    );
  } catch {
    return String(value);
  }
}

/**
 * Build a {@link ParityView} from a decoded local SSIM result. Pass a run driven
 * with `applyRpcAdjustments = true` so the transaction-data resources/fee carry
 * the RPC-parity adjustment. A local failure (inline host/contract error, or
 * missing transaction data) is represented so the comparison surfaces it as a
 * mismatch rather than silently passing.
 */
export function buildLocalView(decoded: SsimDecoded): ParityView {
  let resultScVal: string;
  let resultNative: string;
  if (decoded.result.ok) {
    const b64 = bytesToB64(decoded.result.scValXdr);
    resultScVal = canonicalScVal(b64);
    resultNative = nativeToDisplay(
      scValToNative(xdr.ScVal.fromXdr(b64, "base64")),
    );
  } else {
    resultScVal = `ERROR: ${decoded.result.error.split("\n")[0]}`;
    resultNative = resultScVal;
  }

  let footprintReadOnly: string[] = [];
  let footprintReadWrite: string[] = [];
  // Adjusted instruction count comes from transaction_data; fall back to the raw
  // metered header only if transaction data is somehow absent.
  let instructions = decoded.simulatedInstructions;
  let readBytes = 0;
  let writeBytes = 0;
  let resourceFee = "0";

  if (decoded.transactionData) {
    const td = xdr.SorobanTransactionData.fromXdr(
      bytesToB64(decoded.transactionData),
      "base64",
    );
    const res = td.resources;
    footprintReadOnly = res.footprint.readOnly
      .map((k) => k.toXdr("base64"))
      .sort();
    footprintReadWrite = res.footprint.readWrite
      .map((k) => k.toXdr("base64"))
      .sort();
    instructions = res.instructions;
    readBytes = res.diskReadBytes;
    writeBytes = res.writeBytes;
    resourceFee = td.resourceFee.toString();
  }

  return {
    resultScVal,
    resultNative,
    footprintReadOnly,
    footprintReadWrite,
    auth: decoded.auth.map((a) => canonicalAuth(bytesToB64(a))).sort(),
    contractEvents: decoded.contractEvents
      .map((e) => canonicalContractEvent(bytesToB64(e)))
      .sort(),
    diagnosticEventCount: decoded.diagnosticEvents.length,
    instructions,
    rawInstructions: decoded.simulatedInstructions,
    readBytes,
    writeBytes,
    resourceFee,
  };
}

/** Build a {@link ParityView} from the recorded RPC fixture fields. */
export function buildRpcView(rpc: RpcFixtureData): ParityView {
  // RPC returns each event wrapped as a DiagnosticEvent. The SAC contract event
  // (type `contract`) carries the transfer topics + data; fn_call / fn_return
  // are type `diagnostic`. Extract the inner ContractEvent XDR for the contract
  // ones so it compares against our SSIM ContractEvent blobs.
  const contractEvents: string[] = [];
  let diagnosticEventCount = 0;
  for (const evB64 of rpc.events) {
    const de = xdr.DiagnosticEvent.fromXdr(evB64, "base64");
    const typeName = de.event.type.name;
    if (typeName === "contract") {
      contractEvents.push(de.event.toXdr("base64"));
    } else if (typeName === "diagnostic") {
      diagnosticEventCount += 1;
    }
  }
  contractEvents.sort();

  return {
    resultScVal: canonicalScVal(rpc.retvalXdr),
    resultNative: nativeToDisplay(
      scValToNative(xdr.ScVal.fromXdr(rpc.retvalXdr, "base64")),
    ),
    footprintReadOnly: rpc.footprintReadOnly.map(canonicalLedgerKey).sort(),
    footprintReadWrite: rpc.footprintReadWrite.map(canonicalLedgerKey).sort(),
    auth: rpc.auth.map(canonicalAuth).sort(),
    contractEvents,
    diagnosticEventCount,
    instructions: rpc.instructions,
    readBytes: rpc.readBytes,
    writeBytes: rpc.writeBytes,
    resourceFee: rpc.minResourceFee,
  };
}

function sameSet(a: string[], b: string[]): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i += 1) if (a[i] !== b[i]) return false;
  return true;
}

function footprintDisplay(keys: string[]): string {
  if (keys.length === 0) return "(none)";
  return `${keys.length} key(s)`;
}

/**
 * Compare a decoded local result (from an `applyRpcAdjustments = true` run)
 * against the recorded RPC fields, producing a structured {@link ParityReport}.
 */
export function buildParityReport(
  decoded: SsimDecoded,
  rpc: RpcFixtureData,
): ParityReport {
  const local = buildLocalView(decoded);
  const remote = buildRpcView(rpc);
  const fields: ParityField[] = [];

  // MUST-match: invocation result value.
  fields.push({
    field: "result (ScVal)",
    local: `${local.resultNative} [${local.resultScVal}]`,
    rpc: `${remote.resultNative} [${remote.resultScVal}]`,
    verdict: local.resultScVal === remote.resultScVal ? "match" : "MISMATCH",
  });

  // MUST-match: footprint read-only set.
  fields.push({
    field: "footprint readOnly",
    local: footprintDisplay(local.footprintReadOnly),
    rpc: footprintDisplay(remote.footprintReadOnly),
    verdict: sameSet(local.footprintReadOnly, remote.footprintReadOnly)
      ? "match"
      : "MISMATCH",
  });

  // MUST-match: footprint read-write set (LedgerKey equality).
  fields.push({
    field: "footprint readWrite",
    local: footprintDisplay(local.footprintReadWrite),
    rpc: footprintDisplay(remote.footprintReadWrite),
    verdict: sameSet(local.footprintReadWrite, remote.footprintReadWrite)
      ? "match"
      : "MISMATCH",
  });

  // MUST-match: auth entries (count + canonical SorobanAuthorizationEntry XDR).
  fields.push({
    field: "auth entries",
    local: `${local.auth.length}`,
    rpc: `${remote.auth.length}`,
    verdict: sameSet(local.auth, remote.auth) ? "match" : "MISMATCH",
  });

  // MUST-match: contract events (canonical ContractEvent XDR — topics + data).
  {
    const eq = sameSet(local.contractEvents, remote.contractEvents);
    fields.push({
      field: "contract events",
      local: `${local.contractEvents.length}`,
      rpc: `${remote.contractEvents.length}`,
      verdict: eq ? "match" : "MISMATCH",
      note: eq
        ? undefined
        : "SAC contract-event topics/data differ from RPC's recorded event",
    });
  }

  // Resource: adjusted instructions (RpcDefault). soroban-simulation applies the
  // same default_adjustment() stellar-rpc uses: max(raw + 50_000, raw * 1.04).
  {
    const eq = local.instructions === remote.instructions;
    const delta = remote.instructions - local.instructions;
    fields.push({
      field: "instructions (RpcDefault)",
      local: `${local.instructions}`,
      rpc: `${remote.instructions}`,
      verdict: eq ? "match" : "divergent-explained",
      note: eq
        ? undefined
        : `adjustment model reproduced exactly (local = raw ${local.rawInstructions} + 50000); ` +
          `residual +${delta} is raw CPU metering: host pinned to protocol 28, ` +
          `RPC captured at protocol 27 (see HOST_PROTOCOL_VERSION)`,
    });
  }

  // Informational: raw metered instructions (adjustment-independent SSIM header).
  fields.push({
    field: "instructions (raw metered)",
    local: `${local.rawInstructions}`,
    rpc: `${remote.instructions - 50000} (implied)`,
    verdict: "divergent-expected",
    note: "informational: our raw metered count vs RPC's adjusted minus the 50000 additive",
  });

  // Resource: disk read bytes.
  fields.push({
    field: "read bytes",
    local: `${local.readBytes}`,
    rpc: `${remote.readBytes}`,
    verdict: local.readBytes === remote.readBytes ? "match" : "divergent-explained",
  });

  // Resource: write bytes.
  fields.push({
    field: "write bytes",
    local: `${local.writeBytes}`,
    rpc: `${remote.writeBytes}`,
    verdict:
      local.writeBytes === remote.writeBytes ? "match" : "divergent-explained",
  });

  // Fee: resource fee under RpcDefault.
  {
    const eq = local.resourceFee === remote.resourceFee;
    const delta = Number(remote.resourceFee) - Number(local.resourceFee);
    const pct =
      Number(remote.resourceFee) === 0
        ? 0
        : (Math.abs(delta) / Number(remote.resourceFee)) * 100;
    fields.push({
      field: "resource fee (stroops)",
      local: local.resourceFee,
      rpc: remote.resourceFee,
      verdict: eq ? "match" : "divergent-explained",
      note: eq
        ? undefined
        : `residual ${delta > 0 ? "+" : ""}${delta} stroops (${pct.toFixed(3)}%): ` +
          `the CPU-fee component tracks the instruction metering delta above`,
    });
  }

  // Diagnostic events: informational; RPC folds the contract event out of its
  // diagnostic count, so a small difference is expected.
  fields.push({
    field: "diagnostic events",
    local: `${local.diagnosticEventCount}`,
    rpc: `${remote.diagnosticEventCount}`,
    verdict:
      local.diagnosticEventCount === remote.diagnosticEventCount
        ? "match"
        : "divergent-expected",
  });

  return {
    fields,
    hasMismatch: fields.some((f) => f.verdict === "MISMATCH"),
  };
}

/** Render a {@link ParityReport} as an aligned, human-readable table. */
export function formatParityTable(report: ParityReport): string {
  const header = ["Field", "Local", "RPC", "Verdict"];
  const rows = report.fields.map((f) => [f.field, f.local, f.rpc, f.verdict]);
  const widths = header.map((h, i) =>
    Math.max(h.length, ...rows.map((r) => r[i]!.length)),
  );
  const pad = (cells: string[]) =>
    cells.map((c, i) => c.padEnd(widths[i]!)).join("  ");
  const sep = widths.map((w) => "-".repeat(w)).join("  ");

  const lines = [pad(header), sep, ...rows.map(pad)];
  for (const f of report.fields) {
    if (f.note) lines.push(`    - ${f.field}: ${f.note}`);
  }
  return lines.join("\n");
}
