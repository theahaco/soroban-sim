import { StrKey, humanizeEvents, type contract, type xdr } from '@stellar/stellar-sdk'

/** A sandbox control (setting a balance, moving time, patching storage) that a local network cannot provide. */
export class SandboxOnlyError extends Error {
  readonly capability: string

  constructor(capability: string) {
    super(
      `${capability} needs soroban-sim: a local network has no way to do it. ` +
        'Skip the test where the suite\'s `sandbox` (or `worker.sandbox`) is false, ' +
        'for example with test.skipIf(!sandbox).',
    )
    this.name = 'SandboxOnlyError'
    this.capability = capability
  }
}

/** A contract call the host refused: a trap, failed authorization, missing entry or exhausted budget. */
export class InvocationError extends Error {
  /** The contract called, `C…`. */
  readonly contract: string
  readonly method: string
  /** The host's error, such as `Error(WasmVm, InvalidAction)` or `Error(Contract, #10)`. */
  readonly hostError: string
  /** The diagnostic events of the failed call, as the RPC returned them. */
  readonly events: xdr.DiagnosticEvent[]

  constructor(target: CallTarget, hostError: string, events: xdr.DiagnosticEvent[], detail: string) {
    super(`${target.method} on ${target.contract} failed: ${hostError}${detail ? `\n\n${detail}` : ''}`)
    this.name = 'InvocationError'
    this.contract = target.contract
    this.method = target.method
    this.hostError = hostError
    this.events = events
  }
}

/** The contract returned one of its own errors (`Error(Contract, #code)`). */
export class ContractError extends InvocationError {
  /** The contract's error code. */
  readonly code: number
  /**
   * The code's name in the called contract's error enum. Undefined when its
   * spec has none, or when a contract it called raised the error and it
   * passed through: the code is then from that contract's enum. Also
   * undefined when the called contract caught a failed call of its own
   * (`try_call`) and then raised its own error with the same number: the
   * name is then missing, never wrong.
   */
  readonly errorName: string | undefined

  constructor(target: CallTarget, code: number, errorName: string | undefined, events: xdr.DiagnosticEvent[], detail: string) {
    super(target, `Error(Contract, #${code})${errorName ? ` ${errorName}` : ''}`, events, detail)
    this.name = 'ContractError'
    this.code = code
    this.errorName = errorName
  }
}

/** Where a transaction stood when it failed or the wait for it ended. */
export interface TransactionDetails {
  /** The transaction's hash, once it was sent. */
  hash?: string
  /** `soroban-sim` or `local`. */
  environment?: string
  /** The last status the network reported for it (`PENDING`, `NOT_FOUND`…). */
  lastStatus?: string
}

/**
 * The network refused or failed a transaction for a reason other than the
 * contract call (a sequence number, a fee), or the wait for it ended first.
 */
export class TransactionError extends Error {
  /**
   * `ERROR` (refused by sendTransaction), `FAILED` (applied and failed),
   * `NOT_FOUND` (not applied within the wait) or `CANCELLED` (the worker was
   * torn down while waiting).
   */
  readonly status: string
  /** The transaction result code, such as `tx_bad_seq`: the SDK's JSON spelling, on every path. */
  readonly result: string | undefined
  readonly hash: string | undefined
  readonly environment: string | undefined
  readonly lastStatus: string | undefined

  constructor(what: string, status: string, result: string | undefined, details: TransactionDetails = {}, why?: string) {
    const where = [
      details.hash ? `transaction ${details.hash}` : undefined,
      details.environment ? `on ${details.environment}` : undefined,
      details.lastStatus ? `last status ${details.lastStatus}` : undefined,
    ].filter(Boolean)
    super(`${what}: ${why ?? `${status}${result ? ` (${result})` : ''}`}${where.length ? ` (${where.join(', ')})` : ''}`)
    this.name = 'TransactionError'
    this.status = status
    this.result = result
    this.hash = details.hash
    this.environment = details.environment
    this.lastStatus = details.lastStatus
  }
}

export interface CallTarget {
  contract: string
  method: string
  spec?: contract.Spec | null
}

const HOST_ERROR = /Error\((\w+), ([^)]+)\)/

/**
 * The typed error for a failed call, from the simulation's error text (which
 * carries the host error and its event log) or, for a transaction that
 * failed on the ledger, from its diagnostic events.
 */
export function invocationError(target: CallTarget, text: string, events: xdr.DiagnosticEvent[]): InvocationError {
  const readable = text.trim() || describeEvents(events)
  const logged = loggedErrors(events)
  const found = HOST_ERROR.exec(text) ?? HOST_ERROR.exec(logged.at(-1)?.error ?? '')
  if (!found) return new InvocationError(target, text.split('\n')[0] || 'unknown host error', events, readable)
  const [, type, value] = found
  const code = /^#(\d+)$/.exec(value)
  if (type === 'Contract' && code) {
    const n = Number(code[1])
    // The contract that raises an error logs it first, so the oldest event with the code is the raiser's;
    // its callers log the same code again as it passes through them.
    const raisedIn = logged.find((e) => e.error === found[0])?.contractId
    return new ContractError(target, n, raisedIn === target.contract ? errorName(target.spec, n) : undefined, events, readable)
  }
  return new InvocationError(target, `Error(${type}, ${value})`, events, readable)
}

function errorName(spec: contract.Spec | null | undefined, code: number): string | undefined {
  try {
    return spec?.errorCases().find((c) => c.value === code)?.name.toString()
  } catch {
    return undefined
  }
}

type Humanized = { topics?: unknown[]; contractId?: string; data?: unknown }

function humanized(events: xdr.DiagnosticEvent[]): Humanized[] {
  try {
    return humanizeEvents(events) as Humanized[]
  } catch {
    return []
  }
}

/** The host errors in the diagnostic `error` events, oldest first, each with the contract that logged it. */
function loggedErrors(events: xdr.DiagnosticEvent[]): { error: string; contractId: string | undefined }[] {
  return events.flatMap(({ event }) => {
    const [topic, logged] = event.body.v0.topics
    if (event.type.name !== 'diagnostic' || topic?.type !== 'scvSymbol' || topic.value !== 'error') return []
    if (logged?.type !== 'scvError') return []
    const { error } = logged
    const value = error.type === 'sceContract' ? `#${error.contractCode}` : error.code.name.slice('scec'.length)
    const contractId = event.contractId ? StrKey.encodeContract(Buffer.from(event.contractId.value)) : undefined
    return [{ error: `Error(${error.type.slice('sce'.length)}, ${value})`, contractId }]
  })
}

function describeEvents(events: xdr.DiagnosticEvent[]): string {
  const json = (v: unknown) => JSON.stringify(v, (_, x) => (typeof x === 'bigint' ? x.toString() : x))
  return humanized(events)
    .map((e, i) => `${i}: ${e.contractId ? `contract:${e.contractId}, ` : ''}topics:${json(e.topics)}, data:${json(e.data)}`)
    .join('\n')
}
