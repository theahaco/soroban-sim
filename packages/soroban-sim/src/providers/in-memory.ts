/**
 * InMemoryStateProvider — a fully in-memory {@link StateProvider} backed by a
 * `Map<keyHex, EntryResolution>`. It carries no network dependency and is the
 * primary provider used to drive the rerun loop deterministically in tests.
 *
 * Tri-state semantics:
 *   - a key mapped to `{ status: "present", entry }` resolves present;
 *   - a key mapped to `{ status: "absent" }` resolves absent (and, once fed
 *     into `knownAbsentKeys`, never re-triggers needs-state);
 *   - a key with NO mapping resolves absent by default (configurable).
 *
 * Keys are compared by the lowercase hex of their LedgerKey XDR bytes, so a
 * caller need not hold the same `Uint8Array` reference it seeded with.
 */
import type { EntryResolution, StateProvider } from "../state-provider.js";

function hex(bytes: Uint8Array): string {
  let out = "";
  for (const b of bytes) out += b.toString(16).padStart(2, "0");
  return out;
}

/** A present entry (LedgerEntry XDR + optional TTL) as seeded into the map. */
export interface InMemoryEntry {
  entryXdr: Uint8Array;
  liveUntilLedger?: number;
}

export interface InMemoryStateProviderOptions {
  /**
   * How to resolve a key that has no explicit mapping. Defaults to `"absent"`
   * (matches the common test scenario where the caller seeds only the keys it
   * expects). Set to `"missing"` to throw instead — useful when a test wants to
   * assert the loop only ever requests keys it has provisioned.
   */
  unmappedPolicy?: "absent" | "missing";
}

export class InMemoryStateProvider implements StateProvider {
  private readonly table = new Map<string, EntryResolution>();
  private readonly unmappedPolicy: "absent" | "missing";

  /**
   * @param entries Initial mapping. Accepts either a `Map`/record of
   *   `keyHex -> EntryResolution`, or an iterable of `[keyXdr, resolution]`
   *   pairs (keyed by the LedgerKey XDR bytes directly).
   */
  constructor(
    entries?:
      | Map<string, EntryResolution>
      | Record<string, EntryResolution>
      | Iterable<readonly [Uint8Array, EntryResolution]>,
    options: InMemoryStateProviderOptions = {},
  ) {
    this.unmappedPolicy = options.unmappedPolicy ?? "absent";
    if (entries) {
      if (entries instanceof Map) {
        for (const [k, v] of entries) this.table.set(k, v);
      } else if (Symbol.iterator in (entries as object)) {
        for (const [keyXdr, res] of entries as Iterable<
          readonly [Uint8Array, EntryResolution]
        >) {
          this.table.set(hex(keyXdr), res);
        }
      } else {
        for (const [k, v] of Object.entries(
          entries as Record<string, EntryResolution>,
        )) {
          this.table.set(k, v);
        }
      }
    }
  }

  /** Seed or overwrite a present entry keyed by its LedgerKey XDR bytes. */
  setPresent(keyXdr: Uint8Array, entry: InMemoryEntry): this {
    this.table.set(hex(keyXdr), { status: "present", entry });
    return this;
  }

  /** Mark a key as known-absent, keyed by its LedgerKey XDR bytes. */
  setAbsent(keyXdr: Uint8Array): this {
    this.table.set(hex(keyXdr), { status: "absent" });
    return this;
  }

  async resolve(keyXdrs: Uint8Array[]): Promise<EntryResolution[]> {
    return keyXdrs.map((key): EntryResolution => {
      const found = this.table.get(hex(key));
      if (found) return found;
      if (this.unmappedPolicy === "missing") {
        throw new Error(
          `InMemoryStateProvider: no mapping for key ${hex(key)} (unmappedPolicy="missing")`,
        );
      }
      return { status: "absent" };
    });
  }
}
