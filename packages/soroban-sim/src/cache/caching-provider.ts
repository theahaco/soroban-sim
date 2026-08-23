/**
 * CachingStateProvider — a {@link StateProvider} decorator that fronts an inner
 * provider with an {@link IndexedDbEntryCache}.
 *
 * On `resolve()` it looks up each requested LedgerKey by a composite cache id,
 * serves cache hits directly (INCLUDING absent markers, so a proven-absent key
 * is never refetched and can't re-trigger the needs-state loop), delegates the
 * misses to the inner provider, and persists every fresh resolution — present
 * AND absent — before returning. The result array stays strictly 1:1 with the
 * input order, as the StateProvider contract requires.
 *
 * Composite cache id (see docs/architecture.md "Cache identity"):
 *
 *     `${networkId}:${ledgerSeq}:${sha256Hex(keyXdr)}`   (+ `:${txHash}` if set)
 *
 * The network id and ledger sequence are part of the id so a value cached for
 * one network or one ledger can never satisfy a lookup for another — different
 * context => different id => guaranteed cache miss. TTLs are copied verbatim
 * from the source; the cache never synthesizes a `liveUntilLedger`.
 */
import type { EntryResolution, StateProvider } from "../state-provider.js";
import {
  CACHE_SCHEMA_VERSION,
  type CachePresentRecord,
  type CacheRecord,
  IndexedDbEntryCache,
} from "./indexeddb.js";

/** Identity inputs that scope every cache id. */
export interface CacheContext {
  /** Network id: 32-byte hash (`LedgerContext.networkId`) or its hex string. */
  networkId: string | Uint8Array;
  /** Ledger sequence the state is being read at. */
  ledgerSeq: number;
  /** Optional transaction hash/id to further scope cached state. */
  txHash?: string;
}

export interface CachingStateProviderOptions {
  /** Provider consulted on cache misses. */
  inner: StateProvider;
  /** Backing cache store. */
  db: IndexedDbEntryCache;
  /** Identity that scopes every cache id. */
  context: CacheContext;
}

function toHex(bytes: Uint8Array): string {
  let out = "";
  for (const b of bytes) out += b.toString(16).padStart(2, "0");
  return out;
}

async function sha256Hex(bytes: Uint8Array): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", bytes as BufferSource);
  return toHex(new Uint8Array(digest));
}

function recordToResolution(record: CacheRecord): EntryResolution {
  if (record.status === "present") {
    const entry: { entryXdr: Uint8Array; liveUntilLedger?: number } = {
      entryXdr: record.entryXdr,
    };
    if (record.liveUntilLedger !== undefined) {
      entry.liveUntilLedger = record.liveUntilLedger;
    }
    return { status: "present", entry };
  }
  return { status: "absent" };
}

function resolutionToRecord(id: string, res: EntryResolution): CacheRecord {
  const storedAt = Date.now();
  if (res.status === "present") {
    const record: CachePresentRecord = {
      id,
      status: "present",
      entryXdr: res.entry.entryXdr,
      schemaVersion: CACHE_SCHEMA_VERSION,
      storedAt,
      byteLength: res.entry.entryXdr.byteLength,
    };
    // Preserve TTL exactly as delivered — never synthesize a value.
    if (res.entry.liveUntilLedger !== undefined) {
      record.liveUntilLedger = res.entry.liveUntilLedger;
    }
    return record;
  }
  return { id, status: "absent", schemaVersion: CACHE_SCHEMA_VERSION, storedAt };
}

export class CachingStateProvider implements StateProvider {
  private readonly inner: StateProvider;
  private readonly db: IndexedDbEntryCache;
  private readonly context: CacheContext;
  private readonly networkIdHex: string;

  constructor(options: CachingStateProviderOptions) {
    this.inner = options.inner;
    this.db = options.db;
    this.context = options.context;
    this.networkIdHex =
      typeof options.context.networkId === "string"
        ? options.context.networkId
        : toHex(options.context.networkId);
  }

  private async cacheId(keyXdr: Uint8Array): Promise<string> {
    const keyHash = await sha256Hex(keyXdr);
    let id = `${this.networkIdHex}:${this.context.ledgerSeq}:${keyHash}`;
    if (this.context.txHash) id += `:${this.context.txHash}`;
    return id;
  }

  async resolve(keyXdrs: Uint8Array[]): Promise<EntryResolution[]> {
    if (keyXdrs.length === 0) return [];

    const ids = await Promise.all(keyXdrs.map((k) => this.cacheId(k)));
    const cached = await this.db.getMany(ids);

    const results = new Array<EntryResolution | undefined>(keyXdrs.length);
    const missIndices: number[] = [];
    const missKeys: Uint8Array[] = [];

    for (let i = 0; i < keyXdrs.length; i += 1) {
      const record = cached[i];
      if (record) {
        results[i] = recordToResolution(record);
      } else {
        missIndices.push(i);
        missKeys.push(keyXdrs[i]!);
      }
    }

    if (missKeys.length > 0) {
      const resolved = await this.inner.resolve(missKeys);
      if (resolved.length !== missKeys.length) {
        throw new Error(
          `CachingStateProvider: inner provider returned ${resolved.length} ` +
            `resolutions for ${missKeys.length} keys (must be 1:1)`,
        );
      }
      const toStore: CacheRecord[] = [];
      for (let j = 0; j < missKeys.length; j += 1) {
        const idx = missIndices[j]!;
        const res = resolved[j]!;
        results[idx] = res;
        toStore.push(resolutionToRecord(ids[idx]!, res));
      }
      await this.db.putMany(toStore);
    }

    return results as EntryResolution[];
  }
}
