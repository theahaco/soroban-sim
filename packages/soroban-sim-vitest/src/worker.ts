import { Address, Asset, Operation, contract, nativeToScVal, rpc, scValToNative, xdr } from '@stellar/stellar-sdk'
import { Account, attach } from './account.ts'
import {
  clientOptions,
  mapSignAndSend,
  type BindingsClass,
  type ContractClient,
  type ContractDefinition,
} from './clients.ts'
import { resolveConfig, SEED, type Config, type EnvironmentKind } from './config.ts'
import { SandboxOnlyError } from './errors.ts'
import { ACCOUNTS, ADMIN, ADOPT, BINDINGS, CLOSED, DEPLOYS } from './internal.ts'
import { RUN_NONCE, deriveKeypair } from './keys.ts'
import type { StellarAssetClient } from './sac.ts'
import { rpcCall, startServer } from './server.ts'

/** Every account starts with this balance on both environments (friendbot's amount). */
export const FUNDING_XLM = '10000'

export interface WorkerOptions {
  /** The default reads SOROBAN_TEST_ENV and the other variables (see the README). */
  config?: Config
  /** soroban-sim: the snapshot the worker's server starts from; the default is the configured base. */
  snapshot?: string
  /**
   * Derives each account's key from its name. On soroban-sim the default is
   * fixed, so the same name is the same account on every run; on a local
   * network it is new for every worker, so workers share no accounts.
   */
  seed?: string
}

/** The next ledger after a time move: its sequence number and close time (Unix seconds). */
export interface NextLedger {
  ledger: number
  closeTime: number
}

interface Backend {
  url: string
  fund(address: string): Promise<void>
  stop(): Promise<void>
}

let localWorkers = 0

/**
 * A network to test on, and the accounts created on it: a `soroban-sim
 * serve` of its own, or a running local network. Create one with
 * `Worker.init()` (or take the `worker` a test receives), and call
 * `tearDown()` when done.
 */
export class Worker {
  readonly environment: EnvironmentKind
  /** True where the sandbox controls work (setBalance, setStorage, advanceLedgers, setTime): on soroban-sim. */
  readonly sandbox: boolean
  readonly rpcUrl: string
  readonly networkPassphrase: string
  /** The RPC client, for anything the worker does not wrap. */
  readonly rpc: rpc.Server
  /** How long `signAndSend` waits for a sent transaction's result (SOROBAN_TEST_WAIT_SECONDS). */
  readonly waitSeconds: number
  readonly #closing = new AbortController()
  readonly #backend: Backend
  readonly #seed: string
  readonly #accounts = new Map<string, Account>();
  /** While setup runs: the hashes of the Wasm the suite lists. */
  [DEPLOYS]: Set<string> | undefined
  /** Generated bindings clients by Wasm hash, from the suite's `wasm`: a client for that code is of that class. */
  readonly [BINDINGS] = new Map<string, BindingsClass>()
  readonly #assets = new Map<string, Promise<string>>()
  /** Pays for asset contract deploys: funded on first use, outside the accounts tests see. */
  #deployer: Promise<Account> | undefined
  /** The deployer sends one transaction at a time: each deploy waits for the one before it. */
  #deploys: Promise<unknown> = Promise.resolve()

  private constructor(
    environment: EnvironmentKind,
    backend: Backend,
    client: rpc.Server,
    passphrase: string,
    seed: string,
    waitSeconds: number,
  ) {
    this.environment = environment
    this.waitSeconds = waitSeconds
    this.sandbox = environment === 'soroban-sim'
    this.rpcUrl = backend.url
    this.networkPassphrase = passphrase
    this.rpc = client
    this.#backend = backend
    this.#seed = seed
  }

  /** Starts a soroban-sim server of the worker's own, or connects to the local network, per the configuration. */
  static async init(options: WorkerOptions = {}): Promise<Worker> {
    const config = options.config ?? resolveConfig()
    const local = config.environment === 'local'
    const backend = local ? localBackend(config) : await simBackend(config, options.snapshot ?? config.sim.snapshot)
    try {
      const client = new rpc.Server(backend.url, { allowHttp: backend.url.startsWith('http:') })
      const { passphrase } = await client.getNetwork()
      const seed = options.seed ?? (local ? `${SEED}/local/${RUN_NONCE}/worker-${localWorkers++}` : SEED)
      return new Worker(config.environment, backend, client, passphrase, seed, config.waitSeconds)
    } catch (e) {
      await backend.stop()
      throw e
    }
  }

  /** Creates an account with a throwaway key derived from `name`, funded with 10,000 XLM. */
  async createAccount(name: string): Promise<Account> {
    if (this.#accounts.has(name)) {
      throw new Error(`account ${JSON.stringify(name)} already exists; use worker.account(${JSON.stringify(name)})`)
    }
    const account = new Account(this, name, deriveKeypair(this.#seed, name))
    this.#accounts.set(name, account)
    try {
      await this.#backend.fund(account.address)
    } catch (e) {
      this.#accounts.delete(name)
      throw e
    }
    return account
  }

  /** An account created earlier on this worker, or by the suite's setup. */
  account(name: string): Account {
    const found = this.#accounts.get(name)
    if (!found) {
      const known = [...this.#accounts.keys()].map((n) => JSON.stringify(n)).join(', ')
      throw new Error(`no account ${JSON.stringify(name)} on this worker (it has ${known || 'none'}); create it with worker.createAccount`)
    }
    return found
  }

  /**
   * An SDK client for a contract already on the network, by its `C…`
   * address, with nobody signing (for reads; `account.client(it)` signs).
   * Given a `defineContract` declaration with a generated client, a client of that class.
   */
  async contract<C extends contract.Client = ContractClient>(id: string, declaration?: ContractDefinition<C>): Promise<C> {
    return (await attach(this, id, declaration?.client)) as unknown as C
  }

  /** An SDK client for the native asset (XLM) contract, deployed first if the network does not have it yet. */
  xlm(): Promise<StellarAssetClient> {
    return this.assetContract(Asset.native())
  }

  /**
   * An SDK client for `asset`'s Stellar asset contract, deployed first if
   * the network does not have it yet (an issued asset's admin is its
   * issuer: `issuer.client(token).mint(…)`).
   */
  async assetContract(asset: Asset): Promise<StellarAssetClient> {
    const key = asset.toString()
    let id = this.#assets.get(key)
    if (!id) {
      id = this.#deploys.then(() => this.#deployAsset(asset))
      this.#deploys = id.catch(() => this.#assets.delete(key))
      this.#assets.set(key, id)
    }
    return (await attach(this, await id, undefined, undefined, null)) as unknown as StellarAssetClient
  }

  #assetDeployer(): Promise<Account> {
    if (!this.#deployer) {
      const account = new Account(this, '\0native-asset-deployer', deriveKeypair(this.#seed, '\0native-asset-deployer'))
      const funded = this.#backend.fund(account.address).then(() => account)
      funded.catch(() => (this.#deployer = undefined))
      this.#deployer = funded
    }
    return this.#deployer
  }

  async #deployAsset(asset: Asset): Promise<string> {
    const id = asset.contractId(this.networkPassphrase)
    if (await this.#exists(id)) return id
    const deployer = await this.#assetDeployer()
    const target = { contract: id, method: 'createStellarAssetContract' }
    try {
      const tx = await contract.AssembledTransaction.buildWithOp(Operation.createStellarAssetContract({ asset }), {
        ...clientOptions(this, id, deployer),
        method: target.method,
        parseResultXdr: (result: xdr.ScVal) => result,
      })
      mapSignAndSend(tx, target, this)
      await tx.signAndSend()
    } catch (e) {
      // Another worker on the same local network may have deployed it first.
      if (!(await this.#exists(id))) throw e
    }
    return id
  }

  async #exists(contractId: string): Promise<boolean> {
    try {
      await this.rpc.getContractInstance(contractId)
      return true
    } catch {
      return false
    }
  }

  /** The latest closed ledger: its sequence number and close time (Unix seconds). */
  async latestLedger(): Promise<{ sequence: number; closeTime: number }> {
    // Read raw: the SDK's parser can lag the network's newest header format.
    const res = await rpcCall<{ sequence: number; closeTime?: string | number }>(this.rpcUrl, 'getLatestLedger')
    return { sequence: res.sequence, closeTime: Number(res.closeTime ?? 0) }
  }

  /** Closes `ledgers` empty ledgers; contract TTLs count down with them. soroban-sim only. */
  async advanceLedgers(ledgers: number): Promise<NextLedger> {
    const res = await this[ADMIN]<{ next: NextLedger }>('advanceLedgers', 'fork_advanceLedgers', { ledgers })
    return res.next
  }

  /** Moves the clock forward so the next ledger closes at `time` (a Date or Unix seconds). soroban-sim only. */
  async setTime(time: Date | number): Promise<NextLedger> {
    const seconds = time instanceof Date ? Math.floor(time.getTime() / 1000) : time
    const res = await this[ADMIN]<{ next: NextLedger }>('setTime', 'fork_setTime', { time: seconds })
    return res.next
  }

  /** Reads one entry of a contract's persistent (default) or temporary storage; undefined when absent. */
  async getStorage<T = any>(contract: ContractRef, key: unknown, options: StorageOptions = {}): Promise<T | undefined> {
    const res = await this.rpc.getLedgerEntries(dataKey(contract, key, options))
    const entry = res.entries[0]
    if (!entry || entry.val.type !== 'contractData') return undefined
    return scValToNative(entry.val.contractData.val) as T
  }

  /** Sets one entry of a contract's storage, keeping its TTL (or, for a new entry, live for a long while). soroban-sim only. */
  async setStorage(contract: ContractRef, key: unknown, value: unknown, options: StorageOptions = {}): Promise<void> {
    const id = idOf(contract)
    const ledgerKey = dataKey(contract, key, options)
    const [current, latest] = await Promise.all([this.rpc.getLedgerEntries(ledgerKey), this.latestLedger()])
    const entry = new xdr.LedgerEntry({
      lastModifiedLedgerSeq: latest.sequence,
      data: xdr.LedgerEntryData.contractData(
        new xdr.ContractDataEntry({
          ext: xdr.ExtensionPoint.v0(),
          contract: new Address(id).toScAddress(),
          key: scVal(key, options.keyType),
          durability: durabilityOf(options),
          val: scVal(value, options.valueType),
        }),
      ),
      ext: xdr.LedgerEntryExt.v0(),
    })
    await this[ADMIN]('setStorage', 'fork_setLedgerEntry', {
      entry: entry.toXdr('base64'),
      liveUntilLedgerSeq: current.entries[0]?.liveUntilLedgerSeq ?? latest.sequence + NEW_ENTRY_TTL,
      label: `${id} storage`,
    })
  }

  /** Removes one entry of a contract's storage. soroban-sim only. */
  async removeStorage(contract: ContractRef, key: unknown, options: StorageOptions = {}): Promise<void> {
    await this[ADMIN]('removeStorage', 'fork_removeLedgerEntry', {
      key: dataKey(contract, key, options).toXdr('base64'),
      label: `${idOf(contract)} storage`,
    })
  }

  /** Stops the worker's soroban-sim server (nothing to stop on a local network). */
  tearDown(): Promise<void> {
    this.#closing.abort()
    return this.#backend.stop()
  }

  /** Aborted by tearDown: a wait for a transaction still running is cancelled. */
  get [CLOSED](): AbortSignal {
    return this.#closing.signal
  }

  /** A soroban-sim admin method; a local network refuses it. */
  async [ADMIN]<T = unknown>(capability: string, method: string, params: Record<string, unknown>): Promise<T> {
    if (!this.sandbox) throw new SandboxOnlyError(capability)
    return rpcCall<T>(this.rpcUrl, method, params)
  }

  /** Accounts that already exist in the state the worker started from (a suite's setup). */
  [ADOPT](names: readonly string[]): void {
    for (const name of names) {
      if (!this.#accounts.has(name)) this.#accounts.set(name, new Account(this, name, deriveKeypair(this.#seed, name)))
    }
  }

  [ACCOUNTS](): string[] {
    return [...this.#accounts.keys()]
  }
}

/** A contract: its client or its `C…` address. */
export type ContractRef = contract.Client | string

export interface StorageOptions {
  /** `persistent` (the default) or `temporary`; instance storage lives in the contract's own entry. */
  durability?: 'persistent' | 'temporary'
  /** How to convert a native key, as `nativeToScVal`'s `type` (`'u32'`, `'symbol'`…); ignored for an xdr.ScVal. */
  keyType?: string
  /** How to convert a native value, as above. */
  valueType?: string
}

/** A new storage entry's TTL in ledgers: about a week of five-second ledgers. */
const NEW_ENTRY_TTL = 120_000

const idOf = (contract: ContractRef) => (typeof contract === 'string' ? contract : contract.options.contractId)

function dataKey(contract: ContractRef, key: unknown, options: StorageOptions): xdr.LedgerKey {
  return xdr.LedgerKey.contractData(
    new xdr.LedgerKeyContractData({
      contract: new Address(idOf(contract)).toScAddress(),
      key: scVal(key, options.keyType),
      durability: durabilityOf(options),
    }),
  )
}

function durabilityOf(options: StorageOptions): xdr.ContractDataDurability {
  return options.durability === 'temporary' ? xdr.ContractDataDurability.temporary : xdr.ContractDataDurability.persistent
}

function scVal(value: unknown, type: string | undefined): xdr.ScVal {
  if (xdr.ScVal.is(value)) return value
  return nativeToScVal(value, type ? ({ type } as Parameters<typeof nativeToScVal>[1]) : undefined)
}

async function simBackend(config: Config, snapshot: string): Promise<Backend> {
  const srv = await startServer(config.sim.bin, ['--snapshot', snapshot])
  return {
    url: srv.url,
    async fund(address) {
      await rpcCall(srv.url, 'fork_setBalance', { account: address, xlm: FUNDING_XLM, label: 'account' })
    },
    stop: () => srv.stop(),
  }
}

function localBackend(config: Config): Backend {
  const { rpcUrl, friendbotUrl } = config.local
  return { url: rpcUrl, fund: (address) => friendbot(friendbotUrl, address), stop: async () => {} }
}

async function friendbot(url: string, account: string): Promise<void> {
  let last = ''
  for (let i = 0, delay = 250; i < 8; i++, delay *= 2) {
    try {
      const res = await fetch(`${url}?addr=${encodeURIComponent(account)}`)
      if (res.ok) return
      last = `HTTP ${res.status} ${(await res.text()).slice(0, 300)}`
      if (res.status < 500 && res.status !== 429) break
    } catch (e) {
      last = `${(e as Error).message}; is a local network running? (stellar container start local)`
    }
    await new Promise((r) => setTimeout(r, delay))
  }
  throw new Error(`friendbot ${url} could not fund ${account}: ${last}`)
}

