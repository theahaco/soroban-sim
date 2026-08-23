/**
 * Differential comparison — decode a local soroban-sim SSIM result and a recorded
 * Stellar RPC `simulateTransaction` response into a common {@link ParityView} and
 * compare them field-by-field.
 *
 * Comparison policy (see `docs/differential-testing.md` "Comparison fields"):
 *   - invocation RESULT (ScVal) ............ MUST match
 *   - FOOTPRINT read-only / read-write sets  MUST match
 *   - AUTH entries ......................... MUST match
 *   - contract EVENTS ...................... MUST match
 *   - RESOURCE estimates (instructions,
 *     read/write bytes) .................... compare; may legitimately diverge
 *   - FEES (resource fee) .................. EXPECTED to diverge — soroban-sim
 *     core uses `SimulationAdjustmentConfig::no_adjustments()` while Stellar RPC
 *     applies conservative fee/instruction adjustments.
 *
 * Each field lands in the report with a {@link Verdict}: `match`,
 * `divergent-expected` (a known, tolerated difference), or `MISMATCH` (a real
 * disagreement that fails the differential test).
 */
import { scValToNative, xdr } from "@stellar/stellar-sdk";
import type { SsimDecoded } from "./decode-ssim.js";

export type Verdict = "match" | "divergent-expected" | "MISMATCH";

/** One compared field in the parity report. */
export interface ParityField {
  field: string;
  local: string;
  rpc: string;
  verdict: Verdict;
  /** Optional human note (e.g. why a divergence is expected). */
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
  /** Number of contract-type events (excludes diagnostic/system events). */
  contractEventCount: number;
  /** Number of diagnostic-type events (fn_call / fn_return, etc.). */
  diagnosticEventCount: number;
  instructions: number;
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
 * Build a {@link ParityView} from a decoded local SSIM result. A local failure
 * (inline host/contract error, or missing transaction data) is represented so
 * the comparison surfaces it as a mismatch rather than silently passing.
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
    contractEventCount: decoded.contractEvents.length,
    diagnosticEventCount: decoded.diagnosticEvents.length,
    instructions,
    readBytes,
    writeBytes,
    resourceFee,
  };
}

/** Build a {@link ParityView} from the recorded RPC fixture fields. */
export function buildRpcView(rpc: RpcFixtureData): ParityView {
  let contractEventCount = 0;
  let diagnosticEventCount = 0;
  for (const evB64 of rpc.events) {
    const de = xdr.DiagnosticEvent.fromXdr(evB64, "base64");
    const typeName = de.event.type.name;
    if (typeName === "contract") contractEventCount += 1;
    else if (typeName === "diagnostic") diagnosticEventCount += 1;
  }

  return {
    resultScVal: canonicalScVal(rpc.retvalXdr),
    resultNative: nativeToDisplay(
      scValToNative(xdr.ScVal.fromXdr(rpc.retvalXdr, "base64")),
    ),
    footprintReadOnly: rpc.footprintReadOnly.map(canonicalLedgerKey).sort(),
    footprintReadWrite: rpc.footprintReadWrite.map(canonicalLedgerKey).sort(),
    auth: rpc.auth.map(canonicalAuth).sort(),
    contractEventCount,
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
 * Compare a decoded local result against the recorded RPC fields, producing a
 * structured {@link ParityReport}.
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

  // MUST-match: footprint read-write set.
  fields.push({
    field: "footprint readWrite",
    local: footprintDisplay(local.footprintReadWrite),
    rpc: footprintDisplay(remote.footprintReadWrite),
    verdict: sameSet(local.footprintReadWrite, remote.footprintReadWrite)
      ? "match"
      : "MISMATCH",
  });

  // MUST-match: auth entries.
  fields.push({
    field: "auth entries",
    local: `${local.auth.length}`,
    rpc: `${remote.auth.length}`,
    verdict: sameSet(local.auth, remote.auth) ? "match" : "MISMATCH",
  });

  // MUST-match: contract events (contract-type only).
  fields.push({
    field: "contract events",
    local: `${local.contractEventCount}`,
    rpc: `${remote.contractEventCount}`,
    verdict:
      local.contractEventCount === remote.contractEventCount
        ? "match"
        : "MISMATCH",
  });

  // Resource estimate: instructions. Divergence is tolerated (no_adjustments).
  fields.push({
    field: "instructions",
    local: `${local.instructions}`,
    rpc: `${remote.instructions}`,
    verdict:
      local.instructions === remote.instructions ? "match" : "divergent-expected",
    note:
      local.instructions === remote.instructions
        ? undefined
        : "core uses no_adjustments(); RPC applies instruction adjustments",
  });

  // Resource estimate: disk read bytes.
  fields.push({
    field: "read bytes",
    local: `${local.readBytes}`,
    rpc: `${remote.readBytes}`,
    verdict: local.readBytes === remote.readBytes ? "match" : "divergent-expected",
  });

  // Resource estimate: write bytes.
  fields.push({
    field: "write bytes",
    local: `${local.writeBytes}`,
    rpc: `${remote.writeBytes}`,
    verdict:
      local.writeBytes === remote.writeBytes ? "match" : "divergent-expected",
  });

  // Fees: expected to diverge (adjustment config differs).
  fields.push({
    field: "resource fee (stroops)",
    local: local.resourceFee,
    rpc: remote.resourceFee,
    verdict: local.resourceFee === remote.resourceFee ? "match" : "divergent-expected",
    note:
      local.resourceFee === remote.resourceFee
        ? undefined
        : "core uses no_adjustments(); RPC applies fee adjustments (minResourceFee)",
  });

  // Diagnostic events: informational; may differ across protocol/build.
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
  const rows = report.fields.map((f) => [
    f.field,
    f.local,
    f.rpc,
    f.verdict,
  ]);
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
