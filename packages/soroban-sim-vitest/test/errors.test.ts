// Which contract a ContractError's name comes from. The events are built in
// the shape the host logs them, so these run without a network.
import { Address, xdr } from '@stellar/stellar-sdk'
import { expect, test } from 'vitest'
import { ContractError } from '../src/index.ts'
import { invocationError } from '../src/errors.ts'
import { StellarAssetClient } from '../src/sac.ts'

const VAULT = 'CA7742EZELDVK5IFOELP7EBOYSFZFU5RO7S5MX4RVUJCYT7U3ZYGMUSV'
const TOKEN = 'CDMLFMKMMD7MWZP3FKUBZPVHTUEDLSX4BYGYKH4GCESXYHS3IHQ4EIG4'

/** A diagnostic `error` event logged while `contract` runs: a contract error code, or another host error. */
function errorIn(contract: string, error: number | xdr.ScError): xdr.DiagnosticEvent {
  const scError = typeof error === 'number' ? xdr.ScError.sceContract(error) : error
  return new xdr.DiagnosticEvent({
    inSuccessfulContractCall: false,
    event: new xdr.ContractEvent({
      ext: xdr.ExtensionPoint.v0(),
      contractId: new xdr.ContractId(Address.fromString(contract).toBuffer()),
      type: xdr.ContractEventType.diagnostic,
      body: xdr.ContractEventBody.v0(
        new xdr.ContractEventV0({
          topics: [xdr.ScVal.scvSymbol('error'), xdr.ScVal.scvError(scError)],
          data: xdr.ScVal.scvVoid(),
        }),
      ),
    }),
  })
}

// The vault's own enum names code 10 here; the token's names it BalanceError.
const { spec } = new StellarAssetClient({
  contractId: VAULT,
  networkPassphrase: 'Standalone Network ; February 2017',
  rpcUrl: 'http://127.0.0.1:1',
  allowHttp: true,
})
const vault = { contract: VAULT, method: 'withdraw', spec }
const paths: [string, string][] = [
  ['simulation', 'HostError: Error(Contract, #10)'],
  ['on the ledger', ''],
]

// The token raises 10 first; the vault logs it twice more, later, as its call fails and its VM traps.
const tokenFailed = [errorIn(TOKEN, 10), errorIn(VAULT, 10), errorIn(VAULT, 10)]
const vaultFailed = [errorIn(VAULT, 10)]
// The token raises 9, the vault catches it (try_call) and then raises its own 10.
const vaultFailedAfterCatching = [errorIn(TOKEN, 9), errorIn(VAULT, 9), errorIn(VAULT, 10)]
// As above, but the token's caught error is also number 10.
const vaultFailedAfterCatchingTheSameNumber = [errorIn(TOKEN, 10), errorIn(VAULT, 10), errorIn(VAULT, 10)]

test.for(paths)('%s: the oldest event with the code names the raiser: an error passing through the called contract is not named from it', ([, text]) => {
  const error = invocationError(vault, text, tokenFailed)
  expect(error).toBeInstanceOf(ContractError)
  expect(error).toMatchObject({ code: 10, errorName: undefined, hostError: 'Error(Contract, #10)' })
})

test.for(paths)('%s: the called contract’s own error is named', ([, text]) => {
  const error = invocationError(vault, text, vaultFailed)
  expect(error).toMatchObject({ code: 10, errorName: 'BalanceError', hostError: 'Error(Contract, #10) BalanceError' })
})

test.for(paths)('%s: the called contract’s own error is named after it caught a failed call', ([, text]) => {
  const error = invocationError(vault, text, vaultFailedAfterCatching)
  expect(error).toMatchObject({ code: 10, errorName: 'BalanceError', hostError: 'Error(Contract, #10) BalanceError' })
})

test.for(paths)('%s: after catching a failed call with the same number, the name is missing, not wrong', ([, text]) => {
  const error = invocationError(vault, text, vaultFailedAfterCatchingTheSameNumber)
  expect(error).toMatchObject({ code: 10, errorName: undefined })
})

test('on the ledger: a host error other than a contract’s keeps its type and code name', () => {
  const trap = xdr.ScError.sceWasmVm(xdr.ScErrorCode.scecInvalidAction)
  const error = invocationError(vault, '', [errorIn(VAULT, trap)])
  expect(error).not.toBeInstanceOf(ContractError)
  expect(error.hostError).toBe('Error(WasmVm, InvalidAction)')
})
