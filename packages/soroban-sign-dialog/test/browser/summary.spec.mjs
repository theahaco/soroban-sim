// The summary names the payments the signer makes in sub-calls and counts the
// other sub-calls, with Details one click away. Trees are the multi-party
// fixture's, and larger ones spliced from its bytes.
import { test, expect } from '@playwright/test';
import { decodeEnvelope, toB64 } from '../../src/xdr.js';
import { capture, multiParty } from '../fixture.mjs';

const M = multiParty.addresses;
const [accountEntry, cosignerEntry] = decodeEnvelope(multiParty.envelope).tx.operations[0].auth;
const PAY = 'Call pay on CDAZ…TCN6';
const SHOP = 'GCUV…5HPY';

const u32 = (n) => { const b = new Uint8Array(4); new DataView(b.buffer).setUint32(0, n); return b; };
const cat = (...parts) => { const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0)); let o = 0; for (const p of parts) { out.set(p, o); o += p.length; } return out; };

// The cosigner's entry is pay(…) { transfer(cosigner → shop, 1 XLM) }. Its
// bytes give a credentials prefix, pay's own call, and a transfer whose
// amount (the i128's low word, before the sub-call count) can be rewritten.
const root = cosignerEntry.root;
const [sub] = root.subs;
const credentials = cosignerEntry.raw.slice(0, cosignerEntry.raw.length - root.raw.length);
const payCall = root.raw.slice(0, root.raw.length - 4 - sub.raw.length);
const pay = (...subs) => cat(payCall, u32(subs.length), ...subs);
const xlm = (n) => { const b = sub.raw.slice(); new DataView(b.buffer).setBigUint64(b.length - 12, BigInt(n) * 10_000_000n); return b; };
const entry = (invocation) => toB64(cat(credentials, invocation));

test.beforeEach(async ({ page }) => {
  await page.goto('/examples/html/');
  await page.evaluate(() => document.querySelector('soroban-sign-dialog')?.remove());
});

async function open(page, opts) {
  return page.evaluate(async (opts) => {
    const { request } = await import('/dist/index.js');
    request(opts);
    await new Promise((r) => setTimeout(r));
  }, { network: capture.network.passphrase, ...opts });
}
const lines = (page) => page.locator('soroban-sign-dialog [part="what"] li');

test('a payment in a nested sub-call is named; a call without sub-calls reads as before', async ({ page }) => {
  await open(page, { transaction: multiParty.envelope, signer: M.cosigner });
  await expect(lines(page)).toHaveText([PAY, `${PAY}, paying 1 XLM to ${SHOP}`]);
  await expect(page.locator('soroban-sign-dialog [part="summary-more"]')).toHaveCount(0);
});

test('another signer’s payment is not named, only counted', async ({ page }) => {
  await open(page, { transaction: multiParty.envelope, signer: M.account });
  await expect(lines(page)).toHaveText([PAY, `${PAY} and 1 more call`]);
});

test('several payments are named, the rest counted, and the count opens Details', async ({ page }) => {
  await open(page, { authEntries: [entry(pay(xlm(1), pay(xlm(2)), xlm(3), xlm(4)))], signer: M.cosigner });
  await expect(lines(page)).toHaveText([`${PAY}, paying 1 XLM to ${SHOP}, 2 XLM to ${SHOP} and 3 XLM to ${SHOP}, and 2 more calls`]);
  const details = page.locator('soroban-sign-dialog [part="details"]');
  await expect(details).not.toHaveAttribute('open');
  await page.locator('soroban-sign-dialog [part="summary-more"]').click();
  await expect(details).toHaveAttribute('open');
  await expect(page.locator('soroban-sign-dialog [part="details-summary"]')).toBeFocused();
  await expect(page.locator('soroban-sign-dialog [part="tree"] [part="node"]')).toHaveCount(6);
});

test('non-token sub-calls are counted in "more"', async ({ page }) => {
  await open(page, { authEntries: [entry(pay(pay(), pay(xlm(5))))], signer: M.cosigner });
  await expect(lines(page)).toHaveText([`${PAY}, paying 5 XLM to ${SHOP}, and 2 more calls`]);
  await open(page, { authEntries: [entry(pay(pay(), pay()))], signer: M.cosigner });
  await expect(lines(page)).toHaveText([`${PAY} and 2 more calls`]);
});

test('the account entry alone is unchanged', async ({ page }) => {
  await open(page, { authEntries: [toB64(accountEntry.raw)], signer: M.account });
  await expect(lines(page)).toHaveText([PAY]);
});
