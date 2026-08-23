/**
 * StateProvider abstraction (M2 #8 implements concrete providers).
 *
 * A StateProvider resolves ledger state for a batch of LedgerKey XDR blobs.
 * The result array MUST be 1:1 with the input order: `resolve(keys)[i]` is the
 * resolution for `keys[i]`. This tri-state contract (present / absent) is what
 * lets the rerun loop feed present entries into `ledgerEntries` and absent keys
 * into `knownAbsentKeys` so a proven-absent key never re-triggers needs-state.
 */

/** Resolution for a single requested LedgerKey. */
export type EntryResolution =
  | {
      status: "present";
      entry: {
        /** stellar-xdr LedgerEntry bytes. */
        entryXdr: Uint8Array;
        /** Required for ContractData/ContractCode entries; the host aborts without it. */
        liveUntilLedger?: number;
      };
    }
  | { status: "absent" };

export interface StateProvider {
  /**
   * Resolve a batch of LedgerKey XDR blobs.
   * @returns resolutions 1:1 with `keyXdrs` (same length, same order).
   */
  resolve(keyXdrs: Uint8Array[]): Promise<EntryResolution[]>;
}
