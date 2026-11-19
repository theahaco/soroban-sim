import { Account } from './account.ts'
import { boundOf } from './clients.ts'
import { MARKER } from './internal.ts'

/**
 * A value as stored: JSON, with tagged objects for what JSON cannot hold.
 * Setup's result and `cacheInputs` go through this on both environments,
 * cached or not, so a test sees the same values either way.
 */
export type Stored = null | boolean | number | string | Stored[] | { [key: string]: Stored }

const SUPPORTED = 'JSON values, bigints, bytes (Uint8Array or Buffer), accounts and contract clients'

/**
 * Encodes `value` for storage, rejecting what cannot be stored with its
 * property path (`result.amounts[2]`). `undefined` object properties are
 * left out, as JSON leaves them out.
 */
export function encode(value: unknown, path = 'result'): Stored | undefined {
  if (value === undefined) return undefined
  if (value === null || typeof value === 'boolean' || typeof value === 'string') return value
  if (typeof value === 'number') {
    if (Number.isFinite(value)) return value
    throw unsupported(path, String(value))
  }
  if (typeof value === 'bigint') return { [MARKER]: 'bigint', value: value.toString() }
  if (value instanceof Uint8Array) return { [MARKER]: 'bytes', base64: Buffer.from(value).toString('base64') }
  if (value instanceof Account) return value.toJSON() as Stored
  if (typeof value === 'object' && boundOf(value as never)) return (value as { toJSON(): Stored }).toJSON()
  if (Array.isArray(value)) {
    return value.map((item, i) => {
      if (item === undefined) throw unsupported(`${path}[${i}]`, 'undefined')
      return encode(item, `${path}[${i}]`) as Stored
    })
  }
  if (typeof value === 'object') {
    const proto = Object.getPrototypeOf(value)
    if (proto !== Object.prototype && proto !== null) throw unsupported(path, describe(value))
    const out: { [key: string]: Stored } = {}
    for (const [key, item] of Object.entries(value)) {
      const encoded = encode(item, `${path}${/^[A-Za-z_$][\w$]*$/.test(key) ? `.${key}` : `[${JSON.stringify(key)}]`}`)
      if (encoded !== undefined) out[key] = encoded
    }
    return out
  }
  throw unsupported(path, typeof value)
}

/** Decodes a stored value; accounts and clients become what `bind` returns for their markers. */
export async function decode(stored: Stored | undefined, bind: (marker: Record<string, Stored>) => Promise<unknown> | unknown): Promise<unknown> {
  if (stored === undefined || stored === null || typeof stored !== 'object') return stored
  if (Array.isArray(stored)) return Promise.all(stored.map((item) => decode(item, bind)))
  switch (stored[MARKER]) {
    case 'bigint':
      return BigInt(stored.value as string)
    case 'bytes':
      return Buffer.from(stored.base64 as string, 'base64')
    case 'account':
    case 'contract':
      return bind(stored)
  }
  const entries = await Promise.all(Object.entries(stored).map(async ([k, v]) => [k, await decode(v, bind)] as const))
  return Object.fromEntries(entries)
}

function describe(value: object): string {
  const name = Object.getPrototypeOf(value)?.constructor?.name
  return name ? `a ${name}` : 'an object with a custom prototype'
}

function unsupported(path: string, what: string): Error {
  return new Error(`${path} is ${what}, which cannot be stored: return ${SUPPORTED}`)
}
