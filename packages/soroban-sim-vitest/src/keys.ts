import { createHash, randomBytes } from 'node:crypto'
import { Keypair } from '@stellar/stellar-sdk'

/**
 * A throwaway keypair derived from `seed` and the account's name: the same
 * seed and name give the same key on every run, so a cached snapshot and the
 * tests started from it agree on who each account is.
 */
export function deriveKeypair(seed: string, name: string): Keypair {
  const raw = createHash('sha256').update(`soroban-sim-vitest/signer\0${seed}\0${name}`).digest()
  return Keypair.fromRawEd25519Seed(raw)
}

/**
 * Mixed into the seeds of local-network accounts: new for every Vitest
 * worker (process or thread) and every run, so no two of them derive the
 * same account on a network that keeps them all.
 */
export const RUN_NONCE = randomBytes(8).toString('hex')
