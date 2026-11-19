// signAndSend signs with the client's far time bound, so time travel never
// makes a transaction too late, but waits for the result only as long as the
// worker says, and stops waiting when the worker is torn down.
import { contract, xdr } from '@stellar/stellar-sdk'
import { expect, test } from 'vitest'
import { CLIENT_TIMEOUT_SECONDS, TransactionError } from '../src/index.ts'
import { mapSignAndSend, type WaitContext } from '../src/clients.ts'
import { CLOSED } from '../src/internal.ts'

/** The parts of an AssembledTransaction signAndSend uses, recording the bound in force when it signs and when it polls. */
function fakeTransaction(outcome: 'pending' | 'hang' | 'unsubmitted' | 'refused' | 'failed') {
  const seen: { signedWith?: number; polledWith?: number } = {}
  const tx = {
    options: { timeoutInSeconds: CLIENT_TIMEOUT_SECONDS },
    async signAndSend({ watcher }: { watcher?: contract.Watcher } = {}) {
      seen.signedWith = tx.options.timeoutInSeconds
      if (outcome === 'refused') {
        const refusal = { status: 'ERROR', hash: 'abc123', errorResult: { result: 'tx_too_late' } }
        throw new contract.SentTransaction.Errors.SendFailed(`Sending the transaction to the network failed!\n${JSON.stringify(refusal, null, 2)}`)
      }
      if (outcome === 'unsubmitted') return new Promise(() => {})
      watcher?.onSubmitted?.({ hash: 'abc123', status: 'PENDING' } as never)
      seen.polledWith = tx.options.timeoutInSeconds
      watcher?.onProgress?.({ status: 'NOT_FOUND' } as never)
      if (outcome === 'hang') return new Promise(() => {})
      if (outcome === 'failed') {
        // A transaction result as the network returns it: txFailed, no operation results.
        const resultXdr = xdr.TransactionResult.fromXdr('AAAAAAAAAAD/////AAAAAAAAAAA=', 'base64')
        return { getTransactionResponse: { status: 'FAILED', diagnosticEventsXdr: [], resultXdr } }
      }
      throw new contract.SentTransaction.Errors.TransactionStillPending('Waited, but it did not complete')
    },
  }
  return { tx: tx as unknown as contract.AssembledTransaction<unknown>, seen }
}

const target = { contract: 'CABC', method: 'put_persist' }
const wait = (closed = new AbortController()): WaitContext => ({ environment: 'local', waitSeconds: 7, [CLOSED]: closed.signal })

test('signs with the far bound, polls with the wait bound, and reports a transaction that never lands', async () => {
  const { tx, seen } = fakeTransaction('pending')
  mapSignAndSend(tx, target, wait())
  const error = await tx.signAndSend().catch((e: unknown) => e)
  expect(seen).toEqual({ signedWith: CLIENT_TIMEOUT_SECONDS, polledWith: 7 })
  expect(tx.options.timeoutInSeconds).toBe(CLIENT_TIMEOUT_SECONDS)
  expect(error).toBeInstanceOf(TransactionError)
  expect(error).toMatchObject({ status: 'NOT_FOUND', hash: 'abc123', environment: 'local', lastStatus: 'NOT_FOUND' })
  expect((error as Error).message).toMatch(/^put_persist on CABC: not applied within 7 s of waiting \(transaction abc123, on local, last status NOT_FOUND\)$/)
})

test('tearing the worker down cancels the wait', async () => {
  const { tx } = fakeTransaction('hang')
  const closed = new AbortController()
  mapSignAndSend(tx, target, wait(closed))
  const sending = tx.signAndSend().catch((e: unknown) => e)
  closed.abort()
  expect(await sending).toMatchObject({ status: 'CANCELLED', hash: 'abc123' })
  expect(String(await sending)).toMatch(/cancelled: the worker was torn down/)
})

test('a transaction the network refuses reports where it was sent, like one that never lands', async () => {
  const { tx } = fakeTransaction('refused')
  mapSignAndSend(tx, target, wait())
  const error = await tx.signAndSend().catch((e: unknown) => e)
  expect(error).toBeInstanceOf(TransactionError)
  expect(error).toMatchObject({ status: 'ERROR', result: 'tx_too_late', hash: 'abc123', environment: 'local', lastStatus: 'ERROR' })
  expect((error as Error).message).toBe('put_persist on CABC: ERROR (tx_too_late) (transaction abc123, on local, last status ERROR)')
  expect((error as Error).cause).toBeInstanceOf(contract.SentTransaction.Errors.SendFailed)
})

test('a transaction applied and failed reports its result code in the same spelling as a refused one', async () => {
  const { tx } = fakeTransaction('failed')
  mapSignAndSend(tx, target, wait())
  const error = await tx.signAndSend().catch((e: unknown) => e)
  expect(error).toBeInstanceOf(TransactionError)
  expect(error).toMatchObject({ status: 'FAILED', result: 'tx_failed', hash: 'abc123', environment: 'local', lastStatus: 'FAILED' })
})

// These two wait on real time: the harness's timer fires a couple of seconds past the bound.
const briefly = (): WaitContext => ({ ...wait(), waitSeconds: 0.05 })

test('the wait ends after the wait bound even when the SDK’s poll ignores it', async () => {
  const { tx } = fakeTransaction('hang')
  mapSignAndSend(tx, target, briefly())
  const error = await tx.signAndSend().catch((e: unknown) => e)
  expect(error).toBeInstanceOf(TransactionError)
  expect(error).toMatchObject({ status: 'NOT_FOUND', hash: 'abc123', environment: 'local', lastStatus: 'NOT_FOUND' })
  expect((error as Error).message).toBe('put_persist on CABC: not applied within 0.05 s of waiting (transaction abc123, on local, last status NOT_FOUND)')
  expect(tx.options.timeoutInSeconds).toBe(CLIENT_TIMEOUT_SECONDS)
})

test('the wait also ends when the transaction is never reported as submitted', async () => {
  const { tx } = fakeTransaction('unsubmitted')
  mapSignAndSend(tx, target, briefly())
  const error = await tx.signAndSend().catch((e: unknown) => e)
  expect(error).toBeInstanceOf(TransactionError)
  expect(error).toMatchObject({ status: 'NOT_FOUND', hash: undefined, lastStatus: 'not sent' })
})
