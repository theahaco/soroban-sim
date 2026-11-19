import { xdr, type contract } from '@stellar/stellar-sdk'
import { STELLAR_ASSET_ERRORS } from './generated/stellar-asset-errors.ts'
import { Client } from './generated/stellar-asset.ts'

// A Stellar asset contract has no Wasm and so no spec to read. Its interface
// is the client `stellar contract bindings typescript` generates from
// soroban-sdk's (src/generated/stellar-asset.ts). Its error names and codes
// are emitted from the host's own enum (src/generated/stellar-asset-errors.ts).

const ERRORS = xdr.ScSpecEntry.scSpecEntryUdtErrorEnumV0(
  new xdr.ScSpecUdtErrorEnumV0({
    doc: '',
    lib: '',
    name: 'Error',
    cases: STELLAR_ASSET_ERRORS.map(([name, value]) => new xdr.ScSpecUdtErrorEnumCaseV0({ doc: '', name, value })),
  }),
)

/** A Stellar asset contract's client: the generated bindings' class, its spec also naming the host's errors. */
export class StellarAssetClient extends Client {
  constructor(options: contract.ClientOptions) {
    super(options)
    this.spec.entries.push(ERRORS)
  }
}
