/**
 * SSIM decoder — the TypeScript mirror of `encode_simulation_result` in
 * `crates/soroban-sim-core/src/lib.rs`.
 *
 * The component's `outcome::complete(bytes)` payload is a deterministic,
 * self-describing binary framing (the "SSIM0" format) of a soroban-simulation
 * `InvokeHostFunctionSimulationResult`. This decoder reads that framing back
 * into raw XDR byte slices so the differential harness can compare a local run
 * field-by-field against a recorded Stellar RPC `simulateTransaction` response.
 *
 * Framing (all integers big-endian), byte-for-byte matching the Rust encoder:
 *
 *   magic:                b"SSIM0\0"                       (6 bytes)
 *   simulated_instructions: u32
 *   simulated_memory:       u32
 *   result:
 *     tag: u8  (0 = success ScVal, 1 = inline contract/host failure)
 *     tag==0 -> length-prefixed ScVal XDR      (u32 len + bytes)
 *     tag==1 -> length-prefixed UTF-8 error msg (u32 len + bytes)
 *   auth:              u32 count + [u32 len + SorobanAuthorizationEntry XDR]*
 *   contract_events:   u32 count + [u32 len + ContractEvent XDR]*
 *   diagnostic_events: u32 count + [u32 len + DiagnosticEvent XDR]*
 *   transaction_data:  u8 present (0/1); if 1 -> u32 len + SorobanTransactionData XDR
 *   modified_entries:  u32 count + [optional(state_before) optional(state_after)]*
 *       optional(entry): u8 present (0/1); if 1 -> u32 len + LedgerEntry XDR
 */

/** The magic prefix every SSIM payload starts with (`"SSIM0\0"`). */
export const SSIM_MAGIC = new Uint8Array([0x53, 0x53, 0x49, 0x4d, 0x30, 0x00]);

/** Successful invocation: the returned value as raw `ScVal` XDR. */
export interface SsimResultOk {
  ok: true;
  /** Raw `ScVal` XDR bytes (already canonical, as emitted by `ScVal::to_xdr`). */
  scValXdr: Uint8Array;
}

/** Inline failure: a host/contract error, encoded as the Rust `{err:?}` string. */
export interface SsimResultErr {
  ok: false;
  /** Debug-formatted host error string (e.g. `HostError: Error(Context, InternalError)`). */
  error: string;
}

export type SsimResult = SsimResultOk | SsimResultErr;

/** A single post-invocation ledger-entry change (both sides `LedgerEntry` XDR). */
export interface SsimEntryDiff {
  before?: Uint8Array;
  after?: Uint8Array;
}

/** Fully decoded SSIM payload. All XDR fields are returned as raw byte slices. */
export interface SsimDecoded {
  simulatedInstructions: number;
  simulatedMemory: number;
  result: SsimResult;
  /** `SorobanAuthorizationEntry` XDR blobs, in emission order. */
  auth: Uint8Array[];
  /** `ContractEvent` XDR blobs, in emission order. */
  contractEvents: Uint8Array[];
  /** `DiagnosticEvent` XDR blobs, in emission order (populated when diagnostics enabled). */
  diagnosticEvents: Uint8Array[];
  /** `SorobanTransactionData` XDR (footprint + resources + resourceFee); absent on failure. */
  transactionData?: Uint8Array;
  /** Post-invocation `LedgerEntry` diffs. */
  modifiedEntries: SsimEntryDiff[];
}

class Cursor {
  private off = 0;
  private readonly view: DataView;

  constructor(private readonly data: Uint8Array) {
    this.view = new DataView(data.buffer, data.byteOffset, data.byteLength);
  }

  private ensure(n: number): void {
    if (this.off + n > this.data.length) {
      throw new Error(
        `SSIM decode overrun: need ${n} byte(s) at offset ${this.off}, only ${
          this.data.length - this.off
        } remain`,
      );
    }
  }

  u8(): number {
    this.ensure(1);
    const v = this.view.getUint8(this.off);
    this.off += 1;
    return v;
  }

  u32(): number {
    this.ensure(4);
    const v = this.view.getUint32(this.off, false);
    this.off += 4;
    return v;
  }

  take(n: number): Uint8Array {
    this.ensure(n);
    // Copy so callers own an independent slice (the backing buffer may be reused).
    const out = this.data.slice(this.off, this.off + n);
    this.off += n;
    return out;
  }

  /** A length-prefixed byte blob: u32 length then that many bytes. */
  lenPrefixed(): Uint8Array {
    return this.take(this.u32());
  }

  remaining(): number {
    return this.data.length - this.off;
  }
}

function readVec(cursor: Cursor): Uint8Array[] {
  const count = cursor.u32();
  const out: Uint8Array[] = [];
  for (let i = 0; i < count; i += 1) out.push(cursor.lenPrefixed());
  return out;
}

function readOptionalEntry(cursor: Cursor): Uint8Array | undefined {
  return cursor.u8() === 1 ? cursor.lenPrefixed() : undefined;
}

function utf8Decode(bytes: Uint8Array): string {
  return new TextDecoder().decode(bytes);
}

/**
 * Decode an SSIM `complete` payload. Throws if the magic is wrong or the framing
 * is truncated/overruns — a decode failure means the encoder and this decoder
 * have drifted, which is itself a bug worth surfacing loudly.
 */
export function decodeSsim(bytes: Uint8Array): SsimDecoded {
  const cursor = new Cursor(bytes);
  const magic = cursor.take(SSIM_MAGIC.length);
  for (let i = 0; i < SSIM_MAGIC.length; i += 1) {
    if (magic[i] !== SSIM_MAGIC[i]) {
      throw new Error(
        `SSIM decode: bad magic ${[...magic]
          .map((b) => b.toString(16).padStart(2, "0"))
          .join("")}`,
      );
    }
  }

  const simulatedInstructions = cursor.u32();
  const simulatedMemory = cursor.u32();

  const resultTag = cursor.u8();
  let result: SsimResult;
  if (resultTag === 0) {
    result = { ok: true, scValXdr: cursor.lenPrefixed() };
  } else if (resultTag === 1) {
    result = { ok: false, error: utf8Decode(cursor.lenPrefixed()) };
  } else {
    throw new Error(`SSIM decode: unknown result tag ${resultTag}`);
  }

  const auth = readVec(cursor);
  const contractEvents = readVec(cursor);
  const diagnosticEvents = readVec(cursor);

  const transactionData = cursor.u8() === 1 ? cursor.lenPrefixed() : undefined;

  const diffCount = cursor.u32();
  const modifiedEntries: SsimEntryDiff[] = [];
  for (let i = 0; i < diffCount; i += 1) {
    const before = readOptionalEntry(cursor);
    const after = readOptionalEntry(cursor);
    modifiedEntries.push({ before, after });
  }

  if (cursor.remaining() !== 0) {
    throw new Error(
      `SSIM decode: ${cursor.remaining()} trailing byte(s) after a full parse — framing drift`,
    );
  }

  return {
    simulatedInstructions,
    simulatedMemory,
    result,
    auth,
    contractEvents,
    diagnosticEvents,
    transactionData,
    modifiedEntries,
  };
}
