import { createHash, randomBytes } from 'node:crypto'
import { readFile } from 'node:fs/promises'
import { Address, Operation, StrKey, contract, hash, xdr, type Keypair } from '@stellar/stellar-sdk'
import {
  boundOf,
  buildClient,
  clientOptions,
  isDefinition,
  mapSignAndSend,
  mapping,
  rememberSpec,
  specFor,
  wasmHashOf,
  type BindingsClass,
  type ContractClient,
  type ContractDefinition,
} from './clients.ts'
import { ADMIN, BINDINGS, DEPLOYS, MARKER } from './internal.ts'
import { StellarAssetClient } from './sac.ts'
import { toPath } from './paths.ts'
import type { Worker } from './worker.ts'

export interface DeployOptions {
  /** The constructor's arguments, named, as the contract's spec declares them. */
  args?: Record<string, unknown>
  /** 32 bytes; the default is random, so each deploy is a new contract. */
  salt?: Uint8Array
}

/** A funded account, created by name. It signs and pays for the transactions of the clients bound to it. */
export class Account {
  readonly worker: Worker
  /** The name the account was created with. */
  readonly name: string
  /** Its throwaway keypair, for building transactions by hand. */
  readonly keypair: Keypair

  constructor(worker: Worker, name: string, keypair: Keypair) {
    this.worker = worker
    this.name = name
    this.keypair = keypair
  }

  /** `G…` */
  get address(): string {
    return this.keypair.publicKey()
  }

  /** The account as an SDK signer: `signTransaction` and `signAuthEntry` (`contract.basicNodeSigner`). */
  get signer(): ReturnType<typeof contract.basicNodeSigner> {
    return contract.basicNodeSigner(this.keypair, this.worker.networkPassphrase)
  }

  /**
   * The same contract with this account signing: an SDK client of the same
   * class (the generated one, or `contract.Client`), on this account's
   * network.
   */
  client<C extends contract.Client>(contract: C): C {
    const bound = boundOf(contract)
    if (!bound) throw new Error('account.client takes a client from this package (deploy, worker.contract or worker.xlm)')
    if (bound.worker !== this.worker) throw new Error(`that client is on another worker's network than ${this.name}`)
    return buildClient(this.worker, contract.options.contractId, bound.wasmHash, this, bound.Bindings ?? contract.spec) as unknown as C
  }

  /**
   * Uploads the contract's Wasm, creates a contract from it with `args` for
   * its constructor, and returns a client for it with this account signing.
   * Given a `defineContract` declaration with a generated client, a client
   * of that class, typed; given a path, URL or bytes, the class the suite's
   * `wasm` declares for that code, or the spec-built `contract.Client`.
   */
  deploy<C extends contract.Client>(contract: ContractDefinition<C>, options?: DeployOptions): Promise<C>
  deploy(wasm: string | URL | Uint8Array, options?: DeployOptions): Promise<ContractClient>
  async deploy(
    what: string | URL | Uint8Array | ContractDefinition<contract.Client>,
    options: DeployOptions = {},
  ): Promise<contract.Client> {
    const wasm = isDefinition(what) ? what.file : what
    const declared = isDefinition(what) ? what.client : undefined
    const code = wasm instanceof Uint8Array ? wasm : await readFile(toPath(wasm))
    const wasmHash = createHash('sha256').update(code).digest('hex')
    const allowed = this.worker[DEPLOYS]
    if (allowed && !allowed.has(wasmHash)) {
      const what = wasm instanceof Uint8Array ? `Wasm ${wasmHash.slice(0, 12)}…` : String(wasm)
      throw new Error(`setup deploys ${what}, which the suite does not list in \`wasm\`: list it so the setup cache key covers it`)
    }
    const Bindings = classFor(this.worker, wasmHash, declared, String(wasm))
    rememberSpec(wasmHash, code)
    const salt = options.salt ?? randomBytes(32)
    const id = contractId(this.worker.networkPassphrase, this.address, salt)
    const signed = clientOptions(this.worker, id, this)

    // Code already on the ledger (uploaded by setup, say) is not uploaded again:
    // the SDK takes that upload's simulation for a read and will not send it.
    if (!(await this.worker.rpc.getContractWasmByHash(wasmHash, 'hex').then(() => true, () => false))) {
      const upload = await contract.AssembledTransaction.buildWithOp(Operation.uploadContractWasm({ wasm: Buffer.from(code) }), {
        ...signed,
        method: 'uploadContractWasm',
        parseResultXdr: (result: xdr.ScVal) => result,
      })
      const uploadTarget = { contract: 'the network', method: 'uploadContractWasm' }
      mapSignAndSend(upload, uploadTarget, this.worker)
      if (!(await mapping(uploadTarget, upload, async () => upload.isReadCall))) await upload.signAndSend()
    }

    const spec = await specFor(this.worker, wasmHash)
    const { contractId: _id, ...deployOptions } = signed
    const created = (await contract.Client.deploy(options.args ?? null, {
      ...deployOptions,
      wasmHash,
      salt: Buffer.from(salt),
    })) as contract.AssembledTransaction<unknown>
    const target = { contract: id, method: '__constructor', spec }
    mapSignAndSend(created, target, this.worker)
    await created.signAndSend()
    return buildClient(this.worker, id, wasmHash, this, Bindings ?? spec)
  }

  /**
   * Signs this account's authorization entries in `tx`, a transaction
   * another account sends (`tx.needsNonInvokerSigningBy()` lists who must):
   * the SDK's `signAuthEntries`, valid for the next 100 ledgers.
   */
  async signAuthEntries(tx: contract.AssembledTransaction<unknown>): Promise<void> {
    // The SDK reads the latest ledger itself when no expiration is given, and
    // its parser can lag the network's newest header format.
    const { sequence } = await this.worker.latestLedger()
    await tx.signAuthEntries({ address: this.address, signAuthEntry: this.signer.signAuthEntry, expiration: sequence + 100 })
  }

  /** The account's XLM balance in stroops. */
  async balance(): Promise<bigint> {
    const entry = await this.worker.rpc.getAccountEntry(this.address)
    return BigInt(String(entry.balance))
  }

  /** Sets the account's XLM balance, in stroops. soroban-sim only. */
  async setBalance(stroops: bigint): Promise<void> {
    await this.worker[ADMIN]('setBalance', 'fork_setBalance', { account: this.address, stroops: stroops.toString(), label: this.name })
  }

  /** Stored in a setup's result as a marker; a test gets the account back on its own network. */
  toJSON(): unknown {
    return { [MARKER]: 'account', name: this.name }
  }

  toString(): string {
    return this.address
  }
}

/** An unsigned client for the contract at `id` (reads only): of the bindings class when given, and a Stellar asset contract's is a StellarAssetClient. */
export async function attach(
  worker: Worker,
  id: string,
  Bindings?: BindingsClass,
  signer?: Account,
  knownHash?: string | null,
): Promise<ContractClient> {
  const wasmHash = knownHash === undefined ? await wasmHashOf(worker, id) : (knownHash ?? undefined)
  const declared = classFor(worker, wasmHash, Bindings, id)
  const from = declared ?? (wasmHash === undefined ? StellarAssetClient : await specFor(worker, wasmHash))
  return buildClient(worker, id, wasmHash, signer, from)
}

/**
 * The class of a client for the code `wasmHash` names: the declaration's
 * generated client, else the one the suite's `wasm` pairs with that code.
 * While setup runs the two must be the same, since a test gets a client back
 * by the suite's pairing alone.
 */
function classFor(worker: Worker, wasmHash: string | undefined, declared: BindingsClass | undefined, what: string): BindingsClass | undefined {
  const paired = wasmHash ? worker[BINDINGS].get(wasmHash) : undefined
  if (worker[DEPLOYS] && declared && declared !== paired) {
    throw new Error(
      `setup uses a defineContract declaration with a generated client for ${what}, which the suite does not list in \`wasm\`: ` +
        'list that declaration in `wasm`, so every test gets the same class back',
    )
  }
  return declared ?? paired
}

/** The address of the contract `deployer` creates with `salt`. */
function contractId(networkPassphrase: string, deployer: string, salt: Uint8Array): string {
  const preimage = xdr.HashIdPreimage.envelopeTypeContractId(
    new xdr.HashIdPreimageContractId({
      networkId: hash(Buffer.from(networkPassphrase)),
      contractIdPreimage: xdr.ContractIdPreimage.contractIdPreimageFromAddress(
        new xdr.ContractIdPreimageFromAddress({ address: Address.fromString(deployer).toScAddress(), salt: Buffer.from(salt) }),
      ),
    }),
  )
  return StrKey.encodeContract(hash(Buffer.from(preimage.toXdr())))
}
