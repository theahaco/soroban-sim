// The summary line's account of sub-calls: the signer's payments of known
// tokens, depth first, and a count of everything else, on decoded trees.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { decodeEnvelope, paymentOf, subCallSummary, SUMMARY_PAYMENTS } from '../src/xdr.js';
import { multiParty } from './fixture.mjs';

const SIGNER = 'GSIGNER';
const XLM = { code: 'XLM', decimals: 7 };
const USDC = { code: 'USDC', decimals: 6 };
const TOKENS = { CXLM: XLM, CUSDC: USDC };
const tokenOf = (c) => TOKENS[c] ?? null;

const addr = (str) => ({ t: 'address', str });
const call = (contract, fn, args, subs = []) => ({ fn: { kind: 'call', contract: addr(contract), fn, args }, subs });
const transfer = (contract, from, to, amount, subs = []) => call(contract, 'transfer', [addr(from), addr(to), { t: 'i128', v: amount }], subs);
const create = (subs = []) => ({ fn: { kind: 'create', args: [] }, subs });
const summarize = (root, max) => subCallSummary(root, SIGNER, tokenOf, max);

test('a root call with no sub-calls has nothing to add', () => {
  assert.deepEqual(summarize(call('CROUTER', 'swap', [])), { payments: [], more: 0 });
  assert.deepEqual(summarize(transfer('CXLM', SIGNER, 'GSHOP', 5n)), { payments: [], more: 0 });
});

test('a payment the signer makes in a nested sub-call is named', () => {
  const root = call('CROUTER', 'swap', [], [call('CPOOL', 'deposit', [], [transfer('CUSDC', SIGNER, 'CPOOL', 2_500_000n)])]);
  const s = summarize(root);
  assert.equal(s.payments.length, 1);
  assert.deepEqual(s.payments[0], { token: USDC, amount: 2_500_000n, to: addr('CPOOL') });
  assert.equal(s.more, 1, 'the pool call is counted');
});

test('several payments are named depth first', () => {
  const root = call('CROUTER', 'swap', [], [
    transfer('CXLM', SIGNER, 'GA', 1n, [transfer('CUSDC', SIGNER, 'GB', 2n)]),
    transfer('CXLM', SIGNER, 'GC', 3n),
  ]);
  const s = summarize(root);
  assert.deepEqual(s.payments.map((p) => [p.token.code, p.amount, p.to.str]), [['XLM', 1n, 'GA'], ['USDC', 2n, 'GB'], ['XLM', 3n, 'GC']]);
  assert.equal(s.more, 0);
});

test('payments past the limit are counted with the other calls', () => {
  const pays = Array.from({ length: SUMMARY_PAYMENTS + 2 }, (_, i) => transfer('CXLM', SIGNER, `G${i}`, BigInt(i + 1)));
  const s = summarize(call('CROUTER', 'batch', [], pays));
  assert.equal(s.payments.length, SUMMARY_PAYMENTS);
  assert.deepEqual(s.payments.map((p) => p.amount), [1n, 2n, 3n]);
  assert.equal(s.more, 2);
  assert.deepEqual(summarize(call('CROUTER', 'batch', [], pays), 1), { payments: [s.payments[0]], more: SUMMARY_PAYMENTS + 1 });
});

test('non-token calls, deployments and other people’s transfers are counted, not named', () => {
  const root = call('CROUTER', 'swap', [], [
    call('CORACLE', 'price', []),
    transfer('CUNKNOWN', SIGNER, 'GA', 1n),
    transfer('CXLM', 'GOTHER', 'GA', 1n),
    call('CXLM', 'transfer', [addr(SIGNER), addr('GA'), { t: 'string', v: '1' }]),
    call('CXLM', 'approve', [addr(SIGNER), addr('GA'), { t: 'i128', v: 1n }, { t: 'u32', v: 1 }]),
    create([transfer('CXLM', SIGNER, 'GD', 7n)]),
  ]);
  const s = summarize(root);
  assert.deepEqual(s.payments.map((p) => p.to.str), ['GD'], 'a payment under a deployment is still named');
  assert.equal(s.more, 6);
});

test('without a signer nothing is a payment', () => {
  const root = call('CROUTER', 'swap', [], [transfer('CXLM', SIGNER, 'GA', 1n)]);
  assert.deepEqual(subCallSummary(root, null, tokenOf), { payments: [], more: 1 });
  assert.equal(paymentOf(root.subs[0].fn, null, tokenOf), null);
});

test('the multi-party fixture: the cosigner’s `pay` moves 1 XLM in a sub-call', () => {
  const [, cosigned] = decodeEnvelope(multiParty.envelope).tx.operations[0].auth;
  const A = multiParty.addresses;
  const native = 'CDMLFMKMMD7MWZP3FKUBZPVHTUEDLSX4BYGYKH4GCESXYHS3IHQ4EIG4';
  const known = (c) => (c === native ? XLM : null);
  assert.equal(paymentOf(cosigned.root.fn, A.cosigner, known), null, 'the root is not a payment');
  const s = subCallSummary(cosigned.root, A.cosigner, known);
  assert.deepEqual(s.payments.map((p) => [p.token.code, p.amount, p.to.str]), [['XLM', 10_000_000n, A.shop]]);
  assert.equal(s.more, 0);
  assert.deepEqual(subCallSummary(cosigned.root, A.account, known), { payments: [], more: 1 }, 'not the account’s payment');
});
