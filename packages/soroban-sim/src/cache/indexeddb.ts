/**
 * IndexedDbEntryCache — a thin, dependency-free wrapper around the raw
 * `IndexedDB` API used to persist resolved ledger state across simulate() runs
 * (and across page reloads / worker restarts).
 *
 * It stores a TRI-STATE record per composite cache id:
 *   - `{ status: "present", entryXdr, liveUntilLedger? }` for a resolved entry;
 *   - `{ status: "absent" }` for a proven-absent key.
 * Caching the ABSENT marker is essential: without it a key the network reports
 * as missing would be refetched on every rerun, and the needs-state loop could
 * never terminate cheaply. See {@link CachingStateProvider} for the composite
 * id scheme (`${networkId}:${ledgerSeq}:${sha256(keyXdr)}` + optional txHash).
 *
 * Cache versioning: the schema version is baked into BOTH the physical database
 * name (`<name>/v<N>`) and a `schemaVersion` field on every record. Bumping
 * {@link CACHE_SCHEMA_VERSION} therefore opens a brand-new database and never
 * reads records written by an older, incompatible layout.
 *
 * This wrapper deliberately avoids heavy IndexedDB helper libraries to keep the
 * worker bundle lean; it wraps the request/transaction event API in promises.
 */

/** Schema version. Bump on any incompatible record-shape change. */
export const CACHE_SCHEMA_VERSION = 1;

const DEFAULT_DB_NAME = "soroban-sim-state-cache";
const STORE = "entries";

/** A cached present entry (LedgerEntry XDR + optional TTL). */
export interface CachePresentRecord {
  /** Composite cache id (the object-store key path). */
  id: string;
  status: "present";
  /** stellar-xdr LedgerEntry bytes. */
  entryXdr: Uint8Array;
  /** TTL preserved verbatim from the source; never synthesized. */
  liveUntilLedger?: number;
  schemaVersion: number;
  /** Epoch millis the record was written (for future eviction policy). */
  storedAt: number;
  /** Size of `entryXdr` in bytes (for future size-based eviction). */
  byteLength: number;
}

/** A cached absent marker for a proven-missing key. */
export interface CacheAbsentRecord {
  id: string;
  status: "absent";
  schemaVersion: number;
  storedAt: number;
}

export type CacheRecord = CachePresentRecord | CacheAbsentRecord;

export interface IndexedDbEntryCacheOptions {
  /** Logical database name; the schema version is appended automatically. */
  dbName?: string;
  /**
   * IndexedDB factory to use. Defaults to `globalThis.indexedDB`. Tests inject
   * `fake-indexeddb`'s factory (or import `fake-indexeddb/auto` to set the
   * global). Throws at {@link IndexedDbEntryCache.open} if none is available.
   */
  indexedDB?: IDBFactory;
  /**
   * EVICTION POLICY PLACEHOLDER. When set, marks the intended upper bound on
   * the number of cached records. Eviction itself is NOT yet implemented — see
   * {@link IndexedDbEntryCache.evictIfNeeded}. The `storedAt`/`byteLength`
   * fields are already persisted so a future policy (LRU / oldest-first / size
   * cap) can be layered on without a schema bump.
   */
  maxEntries?: number;
}

function promisifyRequest<T>(req: IDBRequest<T>): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error ?? new Error("IndexedDB request failed"));
  });
}

function promisifyTransaction(tx: IDBTransaction): Promise<void> {
  return new Promise<void>((resolve, reject) => {
    tx.oncomplete = () => resolve();
    tx.onerror = () =>
      reject(tx.error ?? new Error("IndexedDB transaction failed"));
    tx.onabort = () =>
      reject(tx.error ?? new Error("IndexedDB transaction aborted"));
  });
}

export class IndexedDbEntryCache {
  /** Physical database name actually opened (`<name>/v<schemaVersion>`). */
  readonly dbName: string;
  readonly schemaVersion = CACHE_SCHEMA_VERSION;
  private readonly db: IDBDatabase;
  private readonly maxEntries?: number;

  private constructor(db: IDBDatabase, dbName: string, maxEntries?: number) {
    this.db = db;
    this.dbName = dbName;
    this.maxEntries = maxEntries;
  }

  /** Open (creating if needed) the versioned cache database. */
  static async open(
    options: IndexedDbEntryCacheOptions = {},
  ): Promise<IndexedDbEntryCache> {
    const factory = options.indexedDB ?? globalThis.indexedDB;
    if (!factory) {
      throw new Error(
        "IndexedDbEntryCache: no IndexedDB factory available " +
          "(pass options.indexedDB, or import 'fake-indexeddb/auto' in tests)",
      );
    }
    const dbName = `${options.dbName ?? DEFAULT_DB_NAME}/v${CACHE_SCHEMA_VERSION}`;
    const db = await new Promise<IDBDatabase>((resolve, reject) => {
      const req = factory.open(dbName, CACHE_SCHEMA_VERSION);
      req.onupgradeneeded = () => {
        const database = req.result;
        if (!database.objectStoreNames.contains(STORE)) {
          database.createObjectStore(STORE, { keyPath: "id" });
        }
      };
      req.onsuccess = () => resolve(req.result);
      req.onerror = () =>
        reject(req.error ?? new Error("IndexedDbEntryCache: open failed"));
    });
    return new IndexedDbEntryCache(db, dbName, options.maxEntries);
  }

  /** Fetch a single record by composite id. */
  async get(id: string): Promise<CacheRecord | undefined> {
    const tx = this.db.transaction(STORE, "readonly");
    const store = tx.objectStore(STORE);
    return promisifyRequest<CacheRecord | undefined>(store.get(id));
  }

  /** Fetch many records; result is 1:1 with `ids` (undefined = miss). */
  async getMany(ids: string[]): Promise<(CacheRecord | undefined)[]> {
    if (ids.length === 0) return [];
    const tx = this.db.transaction(STORE, "readonly");
    const store = tx.objectStore(STORE);
    return Promise.all(
      ids.map((id) => promisifyRequest<CacheRecord | undefined>(store.get(id))),
    );
  }

  /** Upsert a single record. */
  async put(record: CacheRecord): Promise<void> {
    const tx = this.db.transaction(STORE, "readwrite");
    tx.objectStore(STORE).put(record);
    await promisifyTransaction(tx);
    await this.evictIfNeeded();
  }

  /** Upsert many records in one transaction. */
  async putMany(records: CacheRecord[]): Promise<void> {
    if (records.length === 0) return;
    const tx = this.db.transaction(STORE, "readwrite");
    const store = tx.objectStore(STORE);
    for (const record of records) store.put(record);
    await promisifyTransaction(tx);
    await this.evictIfNeeded();
  }

  /** Number of records currently stored. */
  async count(): Promise<number> {
    const tx = this.db.transaction(STORE, "readonly");
    return promisifyRequest<number>(tx.objectStore(STORE).count());
  }

  /** Remove all records (does not drop the database). */
  async clear(): Promise<void> {
    const tx = this.db.transaction(STORE, "readwrite");
    tx.objectStore(STORE).clear();
    await promisifyTransaction(tx);
  }

  /** Release the underlying connection. */
  close(): void {
    this.db.close();
  }

  /**
   * EVICTION POLICY PLACEHOLDER.
   *
   * Intended behavior (not yet implemented): when `maxEntries` is configured
   * and {@link count} exceeds it, evict records to get back under the cap —
   * e.g. oldest-`storedAt` first (add a `storedAt` index), or a byte-budget
   * variant driven by the persisted `byteLength`. Left as a documented no-op
   * for the spike; the metadata needed to implement any of these policies is
   * already written on every record, so no schema bump is required to add it.
   */
  private async evictIfNeeded(): Promise<void> {
    if (this.maxEntries === undefined) return;
    // Intentionally a no-op for now — see doc comment above.
  }
}
