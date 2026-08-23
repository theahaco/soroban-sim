/**
 * RpcStateProvider — a {@link StateProvider} that resolves ledger state from a
 * Soroban RPC server via `@stellar/stellar-sdk`'s `rpc.Server.getLedgerEntries`.
 *
 * It is constructed from a fully serializable {@link RpcStateProviderConfig}
 * (`{ rpcUrl, networkPassphrase }`) so it can be re-created worker-side after a
 * `postMessage` hop (Topology A: the Worker owns the rerun loop + provider).
 *
 * Contract details honored here:
 *   - `resolve(keyXdrs)` returns an array 1:1 with the input order. RPC's
 *     `getLedgerEntries` omits not-found keys and does not guarantee order, so
 *     results are matched back to inputs by canonical LedgerKey XDR.
 *   - found  -> `{ status: "present", entry: { entryXdr, liveUntilLedger } }`
 *     where `entryXdr` is a full stellar-xdr `LedgerEntry` (RPC returns the
 *     inner `LedgerEntryData` in `val`; we wrap it back into a `LedgerEntry`)
 *     and `liveUntilLedger` preserves the RPC `liveUntilLedgerSeq` TTL.
 *   - not found -> `{ status: "absent" }`.
 */
import { rpc, xdr } from "@stellar/stellar-sdk";
import type { EntryResolution, StateProvider } from "../state-provider.js";

/** Serializable configuration for {@link RpcStateProvider}. */
export interface RpcStateProviderConfig {
  /** Soroban RPC endpoint, e.g. `https://soroban-testnet.stellar.org`. */
  rpcUrl: string;
  /** Network passphrase identifying the network (provider/cache identity). */
  networkPassphrase: string;
  /** Allow plain-http RPC URLs (default false; leave off in production). */
  allowHttp?: boolean;
  /** Optional per-request timeout in seconds forwarded to the RPC client. */
  timeoutSeconds?: number;
}

/**
 * The subset of `rpc.Server` this provider depends on. Exposed so tests can
 * inject a mock without a live network connection.
 */
export interface LedgerEntriesServer {
  getLedgerEntries(
    ...keys: xdr.LedgerKey[]
  ): Promise<rpc.Api.GetLedgerEntriesResponse>;
}

export class RpcStateProvider implements StateProvider {
  readonly rpcUrl: string;
  readonly networkPassphrase: string;
  private readonly server: LedgerEntriesServer;

  /**
   * @param config Serializable connection config.
   * @param server Optional pre-built server (dependency injection for tests).
   *   When omitted, a `rpc.Server` is created from `config`.
   */
  constructor(config: RpcStateProviderConfig, server?: LedgerEntriesServer) {
    this.rpcUrl = config.rpcUrl;
    this.networkPassphrase = config.networkPassphrase;
    this.server =
      server ??
      new rpc.Server(config.rpcUrl, {
        allowHttp: config.allowHttp ?? false,
        timeout: config.timeoutSeconds,
      });
  }

  async resolve(keyXdrs: Uint8Array[]): Promise<EntryResolution[]> {
    if (keyXdrs.length === 0) return [];

    // Decode each input LedgerKey XDR (validates the input as well).
    const decodedKeys = keyXdrs.map((bytes) => xdr.LedgerKey.fromXdr(bytes));

    const response = await this.server.getLedgerEntries(...decodedKeys);

    // Index found entries by canonical base64 LedgerKey (RPC omits misses and
    // does not guarantee input order).
    const found = new Map<string, rpc.Api.LedgerEntryResult>();
    for (const result of response.entries) {
      found.set(result.key.toXdr("base64"), result);
    }

    return decodedKeys.map((key): EntryResolution => {
      const result = found.get(key.toXdr("base64"));
      if (!result) return { status: "absent" };

      // RPC returns the inner LedgerEntryData in `val`; wrap it back into a
      // full LedgerEntry so `entryXdr` matches the WIT `ledger-entry.entry-xdr`
      // contract (stellar-xdr LedgerEntry).
      const entry = new xdr.LedgerEntry({
        lastModifiedLedgerSeq: result.lastModifiedLedgerSeq ?? 0,
        data: result.val,
        ext: xdr.LedgerEntryExt.v0(),
      });

      return {
        status: "present",
        entry: {
          entryXdr: entry.toXdr(),
          liveUntilLedger: result.liveUntilLedgerSeq,
        },
      };
    });
  }
}
