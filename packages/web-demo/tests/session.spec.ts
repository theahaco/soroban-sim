import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { expect, type BrowserContext, type Page, test } from "@playwright/test";
import { Account, Keypair, Networks, Operation, type Transaction, TransactionBuilder, xdr } from "@stellar/stellar-sdk";

/**
 * Network sessions in the browser, offline: the fork's RPC reads are answered
 * from a recording of testnet (tests/fixtures/testnet-fork.json, captured by
 * scripts/capture-session-fixture.mjs). Every key the recording lacks reads
 * as absent, as it does on the network for a fresh throwaway account or a
 * never-uploaded Wasm. Transactions go through the page's virtual RPC (the
 * service worker) into the session; nothing reaches a network.
 */

const TESTNET_RPC = "https://soroban-testnet.stellar.org";
const MAINNET_RPC = "https://mainnet.sorobanrpc.com";
const repo = (path: string) => fileURLToPath(new URL(`../../../${path}`, import.meta.url));

interface Recording {
  getNetwork: { passphrase: string; protocolVersion: number };
  getLatestLedger: { sequence: number; protocolVersion: number; headerXdr: string; id: string };
  entries: Array<{ key: string; xdr: string; lastModifiedLedgerSeq: number; liveUntilLedgerSeq?: number }>;
}

const testnetFork = (): Recording =>
  JSON.parse(readFileSync(new URL("./fixtures/testnet-fork.json", import.meta.url), "utf8")) as Recording;

/** Answer `url`'s JSON-RPC from `recording`; returns the methods it was asked. */
async function recordedRpc(context: BrowserContext, url: string, recording: Recording): Promise<string[]> {
  const asked: string[] = [];
  const entries = new Map(recording.entries.map((e) => [e.key, e]));
  await context.route(`${url}/**`, async (route) => {
    const call = route.request().postDataJSON() as { id: unknown; method: string; params?: { keys?: string[] } };
    asked.push(call.method);
    const latest = recording.getLatestLedger.sequence;
    const result =
      call.method === "getNetwork"
        ? recording.getNetwork
        : call.method === "getLatestLedger"
          ? recording.getLatestLedger
          : call.method === "getLedgerEntries"
            ? { latestLedger: latest, entries: (call.params?.keys ?? []).flatMap((k) => entries.get(k) ?? []) }
            : undefined;
    const body = result
      ? { jsonrpc: "2.0", id: call.id, result }
      : { jsonrpc: "2.0", id: call.id, error: { code: -32601, message: `not recorded: ${call.method}` } };
    await route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(body) });
  });
  return asked;
}

async function fork(page: Page, network: "testnet" | "mainnet"): Promise<void> {
  await page.goto("/");
  await page.locator("#session-network").selectOption(network);
  await expect(page.locator("#session-start")).toBeEnabled();
  await page.locator("#session-start").click();
  await expect(page.locator("#session-state")).toContainText("open:", { timeout: 60_000 });
}

/** One JSON-RPC call to the session's virtual RPC URL, from the page. */
async function virtualRpc(page: Page, method: string, params: unknown = {}): Promise<{ result?: Record<string, unknown>; error?: { code: number; message: string } }> {
  const url = (await page.locator("#session-rpc").textContent())!;
  return page.evaluate(
    async ({ url, method, params }) => {
      const r = await fetch(url, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }) });
      return r.json();
    },
    { url, method, params },
  );
}

const step = (page: Page, n: number) => page.locator(`#session-steps li[data-step="${n}"]`);

test("a two-step dependent sequence runs on a recorded testnet fork and survives a reload", async ({ page, context }) => {
  test.setTimeout(180_000);
  const recording = testnetFork();
  const asked = await recordedRpc(context, TESTNET_RPC, recording);
  await fork(page, "testnet");
  await expect(page.locator("#session-state")).toContainText(`testnet at ledger ${recording.getLatestLedger.sequence}`);
  await expect(page.locator("#session-pill")).toHaveText("testnet fork");
  await expect(page.locator("#session-mainnet-warning")).toBeHidden();
  await expect(step(page, 1)).toContainText("throwaway fee payer");

  // The plan runs through stellar-sdk pointed at the virtual RPC URL: the
  // deploy needs step 3's upload, the call needs step 4's instance, and the
  // first deploy (before the upload) is refused in simulation.
  await page.locator("#session-plan-file").setInputFiles([
    repo("examples/session/two-step-add.yaml"),
    repo("fixtures/contracts/add_i32.wasm"),
  ]);
  await expect(page.locator("#session-plan-status")).toContainText("all as expected", { timeout: 120_000 });
  await expect(page.locator("#session-steps li")).toHaveCount(5);
  await expect(page.locator("#session-step-count")).toHaveText("5 steps · 3 transactions");
  for (const n of [3, 4, 5]) await expect(step(page, n)).toHaveAttribute("data-outcome", "applied");

  // The same contract `soroban-sim session run` derives over the same fork
  // (crates/soroban-sim-cli/tests/plan_run.rs).
  const add = "CDGPQAE2PZSEDRAYUAZMKIB7NXM3IOP3BQACPGPANKLK3BP6OBPADKRW";
  await expect(step(page, 4)).toContainText(`deploy ${add}`);
  await step(page, 4).locator(".session-step-head").click();
  await expect(step(page, 4)).toContainText("created · ContractData");
  await expect(step(page, 4)).toContainText("FEES (XLM) AND RESOURCES", { ignoreCase: true });
  await expect(step(page, 4)).toContainText("source account");
  await step(page, 5).locator(".session-step-head").click();
  await expect(step(page, 5)).toContainText(`${add}.add`);
  await expect(step(page, 5).locator(".session-step-body")).toContainText("42");

  // Any step opens in the time-travel debugger.
  await step(page, 5).getByRole("button", { name: "open in time-travel debugger" }).click();
  await expect(page.locator("#debugger")).toBeVisible({ timeout: 60_000 });
  await expect(page.locator("#dbg-reason")).toBeHidden();
  await expect(page.locator("#dbg-count")).toContainText("semantic steps");
  await expect(page.locator("#dbg-tree")).toContainText("add");

  // stellar-rpc answers from the session itself.
  const latest = await virtualRpc(page, "getLatestLedger");
  expect(latest.result?.sequence).toBe(recording.getLatestLedger.sequence + 3);
  const network = await virtualRpc(page, "getNetwork");
  expect(network.result?.passphrase).toBe(Networks.TESTNET);

  await expect(page.locator("#session-verdict")).toHaveText("passed");
  await expect(page.locator("#session-report-body")).toContainText("Overrides (2)");
  await expect(page.locator("#session-replayable")).toBeHidden();

  // A reload replays the IndexedDB log over the cached fork: same steps, and
  // no base read goes to the network.
  const before = asked.length;
  await page.reload();
  await expect(page.locator("#session-progress")).toContainText("Resumed: replayed", { timeout: 60_000 });
  await expect(page.locator("#session-steps li")).toHaveCount(5);
  await expect(step(page, 5)).toHaveAttribute("data-outcome", "applied");
  expect(asked.slice(before)).toEqual([]);
});

test("a plan with classic-operation steps is refused at import with a pointer to the CLI", async ({ page, context }) => {
  test.setTimeout(120_000);
  await recordedRpc(context, TESTNET_RPC, testnetFork());
  await fork(page, "testnet");
  const before = await page.locator("#session-steps li").count();
  const plan = ["keys: { alice: ed25519, carol: ed25519 }", "steps:", "  - create_account: { destination: \"${key.carol}\", starting_balance: 5 }", "    source: \"${key.alice}\""].join("\n");
  await page.locator("#session-plan-file").setInputFiles({ name: "classic.yaml", mimeType: "text/yaml", buffer: Buffer.from(plan) });
  await expect(page.locator("#session-err")).toContainText("step 1: create_account steps need `soroban-sim session run`");
  // Nothing ran: not even the plan's first step.
  await expect(page.locator("#session-steps li")).toHaveCount(before);
});

test("plan steps sign classic authorization entries with throwaway keys", async ({ page, context }) => {
  test.setTimeout(120_000);
  await recordedRpc(context, TESTNET_RPC, testnetFork());
  await fork(page, "testnet");
  await page.locator("#session-plan-file").setInputFiles(fileURLToPath(new URL("./fixtures/classic-auth.yaml", import.meta.url)));
  await expect(page.locator("#session-plan-status")).toContainText("all as expected", { timeout: 60_000 });
  // payer override, two funding overrides, the SAC deploy, the transfer.
  await expect(page.locator("#session-steps li")).toHaveCount(5);
  await expect(step(page, 5)).toHaveAttribute("data-outcome", "applied");
  await step(page, 5).locator(".session-step-head").click();
  // alice's entry is an address credential (stellar-sdk signs the CAP-71 v2
  // arm) carrying her throwaway signature; carol signed the envelope.
  const alice = "GBEFICSBULGI6ZIMM36MCGPLBWQUKAD6JYNR6N3WQD2JOK6BLP4C7F2J";
  await expect(step(page, 5).locator(".session-step-body")).toContainText(`address_v2 ${alice} · signed`);
  await expect(page.locator("#session-verdict")).toHaveText("passed");
});

test("the wallet pre-submit flow submits into the session through the virtual RPC", async ({ page, context }) => {
  test.setTimeout(180_000);
  const asked = await recordedRpc(context, TESTNET_RPC, testnetFork());
  await fork(page, "testnet");
  const payer = (await page.locator("#session-payer").textContent())!;
  // With a session open, the local identity is the session's throwaway payer.
  await expect(page.locator("#local-identity-address")).toHaveText(payer);
  await expect(page.locator(".identity-card .network-pill")).toHaveText("testnet session");

  // The workbench's guided call, preflighted locally as before.
  await page.locator("#load-contract").click();
  await expect(page.locator("#contract-fn")).toBeVisible({ timeout: 60_000 });
  await page.locator("#contract-fn").selectOption("update_message");
  await expect(page.locator('#contract-args input[data-arg="author"]')).toHaveValue(payer);
  await page.locator("#run").click();
  await expect(page.locator("#review-submit")).toBeEnabled({ timeout: 120_000 });

  await page.locator("#review-submit").click();
  await expect(page.locator("#submit-target")).toBeVisible();
  await expect(page.locator('#submit-target input[value="session"]')).toBeChecked();
  await expect(page.locator("#submit-warning")).toContainText("Nothing is broadcast");
  await expect(page.locator("#replay-warning")).toBeHidden();
  await expect(page.locator("#confirm-submit")).toHaveText("Sign with the throwaway key & submit into the session");
  await page.locator("#confirm-submit").click();
  await expect(page.locator("#submit-progress")).toContainText("Applied in the session", { timeout: 60_000 });
  await page.locator("#submit-dialog .dialog-close").click();

  await expect(step(page, 2)).toHaveAttribute("data-outcome", "applied");
  await expect(step(page, 2)).toContainText("update_message");
  await step(page, 2).locator(".session-step-head").click();
  // The contract instance and code are archived on testnet: the call restored them.
  await expect(step(page, 2).locator(".session-step-body")).toContainText("restored · ContractCode");
  await expect(step(page, 2).locator(".session-step-body")).toContainText("created · ContractData");
  expect(asked).not.toContain("sendTransaction");
});

/** The testnet recording, its base also holding an account the test has the real key of. */
function withRealAccount(real: Keypair, recording = testnetFork()): { recording: Recording; seq: bigint } {
  const seq = BigInt(recording.getLatestLedger.sequence - 10) << 32n;
  const account = xdr.LedgerEntryData.account(
    new xdr.AccountEntry({
      accountId: real.xdrAccountId(),
      balance: xdr.Int64.fromString("5000000000"),
      seqNum: xdr.Int64.fromString(seq.toString()),
      numSubEntries: 0,
      inflationDest: null,
      flags: 0,
      homeDomain: "",
      thresholds: Buffer.from([1, 0, 0, 0]),
      signers: [],
      ext: xdr.AccountEntryExt.v0(),
    }),
  );
  const key = xdr.LedgerKey.account(new xdr.LedgerKeyAccount({ accountId: real.xdrAccountId() }));
  return {
    seq,
    recording: {
      ...recording,
      entries: [...recording.entries, { key: key.toXdr("base64"), xdr: account.toXdr("base64"), lastModifiedLedgerSeq: recording.getLatestLedger.sequence - 10 }],
    },
  };
}

/** The same, as a Mainnet recording. */
function mainnetWithRealAccount(real: Keypair): { recording: Recording; seq: bigint } {
  const recording = testnetFork();
  return withRealAccount(real, { ...recording, getNetwork: { ...recording.getNetwork, passphrase: Networks.PUBLIC } });
}

/** `recording` with its fork ledger closing at `closeTime` (seconds). */
function closingAt(recording: Recording, closeTime: number): Recording {
  const header = Buffer.from(recording.getLatestLedger.headerXdr, "base64");
  // LedgerHeader: ledgerVersion (4), previousLedgerHash (32), txSetHash (32), closeTime.
  header.writeBigUInt64BE(BigInt(closeTime), 68);
  return { ...recording, getLatestLedger: { ...recording.getLatestLedger, headerXdr: header.toString("base64") } };
}

/**
 * A stand-in for the wallet module the page loads on demand: it connects
 * `real`'s account and hands each envelope to `sign`, here in the test.
 */
async function realWallet(page: Page, real: Keypair, sign: (envelope: string, passphrase: string) => string): Promise<void> {
  await page.exposeFunction("__testWalletSign", sign);
  await page.route("**/src/wallet.ts*", (route) =>
    route.fulfill({
      contentType: "text/javascript",
      body: `
        export const NIDO_ID = "nido";
        export async function connectWallet() { return { address: ${JSON.stringify(real.publicKey())}, id: "test", name: "Test wallet" }; }
        export async function disconnectWallet() {}
        export async function signWithWallet(xdr, address, passphrase) {
          return { signedTxXdr: await window.__testWalletSign(xdr, passphrase), signerAddress: address };
        }`,
    }),
  );
}

/** Connect the wallet, run the workbench's guided call, and open the review. */
async function reviewWalletCall(page: Page, real: Keypair): Promise<void> {
  await page.locator("#identity-wallet").click();
  if (!(await page.locator("#wallet-address").isVisible())) await page.locator("#connect-wallet").click();
  await expect(page.locator("#wallet-address")).toHaveText(real.publicKey());
  if (!(await page.locator("#contract-fn").isVisible())) {
    await page.locator("#load-contract").click();
    await expect(page.locator("#contract-fn")).toBeVisible({ timeout: 60_000 });
  }
  await page.locator("#contract-fn").selectOption("update_message");
  await expect(page.locator('#contract-args input[data-arg="author"]')).toHaveValue(real.publicKey());
  await page.locator("#run").click();
  await expect(page.locator("#review-submit")).toBeEnabled({ timeout: 120_000 });
  await page.locator("#review-submit").click();
}

/** The session's own view of itself. */
async function forkInfo(page: Page): Promise<Record<string, unknown>> {
  return (await virtualRpc(page, "fork_info")).result!;
}

test("a real mainnet signer's envelope is refused with a replay warning", async ({ page, context }) => {
  test.setTimeout(120_000);
  // A mainnet fork whose base holds an account the test has the real key of.
  const real = Keypair.random();
  const { recording, seq } = mainnetWithRealAccount(real);
  await recordedRpc(context, MAINNET_RPC, recording);
  await fork(page, "mainnet");
  await expect(page.locator("#session-pill")).toHaveText("mainnet fork");
  await expect(page.locator("#session-mainnet-warning")).toBeVisible();
  await expect(page.locator("#session-mainnet-warning")).toContainText("valid on Mainnet");

  // An upload the real key signs, prepared against the session.
  const wasm = readFileSync(repo("fixtures/contracts/add_i32.wasm"));
  const upload = (fee: string) =>
    new TransactionBuilder(new Account(real.publicKey(), seq.toString()), { fee, networkPassphrase: Networks.PUBLIC })
      .addOperation(Operation.uploadContractWasm({ wasm }))
      .setTimeout(0);
  const sim = await virtualRpc(page, "simulateTransaction", { transaction: upload("100").build().toEnvelope().toXdr("base64") });
  expect(sim.error).toBeUndefined();
  // The resource fee goes on top of the inclusion fee, as stellar-sdk assembles it.
  const prepared = upload(String(100 + Number(sim.result!.minResourceFee)))
    .setSorobanData(String(sim.result!.transactionData))
    .build();
  prepared.sign(real);

  await page.locator(".session-envelope summary").click();
  await page.locator("#session-envelope").fill(prepared.toEnvelope().toXdr("base64"));
  await page.locator("#session-envelope-submit").click();
  await expect(page.locator("#session-err")).toContainText("valid on mainnet", { timeout: 60_000 });
  await expect(step(page, 2)).toHaveAttribute("data-outcome", "refused");
  await expect(page.locator("#session-replayable")).toBeVisible();
  await expect(page.locator("#session-replayable")).toContainText("valid on the real network");
  await expect(page.locator("#session-verdict")).toContainText("FAILED");

  // The real-signed envelope is never stored, so it is not offered either.
  await expect(step(page, 2).locator(".session-outcome")).toHaveText("refused · real-signed, not stored");
  await step(page, 2).locator(".session-step-head").click();
  await expect(step(page, 2).locator(".session-step-body")).toBeVisible();
  await expect(step(page, 2).getByRole("button", { name: "copy envelope XDR" })).toHaveCount(0);
  await page.reload();
  await expect(page.locator("#session-progress")).toContainText("Resumed: replayed 1 logged call", { timeout: 60_000 });
  await expect(page.locator("#session-saved")).toContainText("1 logged");
  await expect(page.locator("#session-steps li")).toHaveCount(1);
});

test("a real wallet signing into a mainnet session must acknowledge the replay warning", async ({ page, context }) => {
  test.setTimeout(180_000);
  const real = Keypair.random();
  // The workbench preflights on Testnet, so the wallet's account is there too.
  await recordedRpc(context, TESTNET_RPC, withRealAccount(real).recording);
  await recordedRpc(context, MAINNET_RPC, mainnetWithRealAccount(real).recording);
  const signed: Array<{ maxTime: number; at: number }> = [];
  await realWallet(page, real, (envelope, passphrase) => {
    const tx = TransactionBuilder.fromXdr(envelope, passphrase) as Transaction;
    signed.push({ maxTime: Number(tx.timeBounds?.maxTime ?? 0), at: Math.floor(Date.now() / 1000) });
    tx.sign(real);
    return tx.toEnvelope().toXdr("base64");
  });
  await fork(page, "mainnet");
  await reviewWalletCall(page, real);

  // The warning names Mainnet and the real account; signing waits for it.
  await expect(page.locator("#replay-warning")).toBeVisible();
  await expect(page.locator("#replay-warning-text")).toContainText("valid on Mainnet");
  await expect(page.locator("#replay-warning-text")).toContainText("your real account");
  await expect(page.locator("#confirm-submit")).toBeDisabled();
  await page.locator("#replay-ack").check();
  await expect(page.locator("#confirm-submit")).toHaveText("Sign with the real wallet & submit into the session");
  await page.locator("#confirm-submit").click();
  await expect(page.locator("#submit-progress")).toContainText("Applied in the session", { timeout: 60_000 });
  await page.locator("#submit-dialog .dialog-close").click();

  // The envelope the wallet signed expires five minutes after it was built.
  expect(signed).toHaveLength(1);
  expect(signed[0]!.maxTime).toBeGreaterThan(signed[0]!.at);
  expect(signed[0]!.maxTime).toBeLessThanOrEqual(signed[0]!.at + 300);

  // Real signers were allowed for that one envelope, then refused again. The
  // report keeps the signature under Safety and fails.
  await expect(step(page, 2)).toContainText("fork_allowRealSigners");
  await expect(step(page, 3)).toHaveAttribute("data-outcome", "applied");
  await expect(step(page, 3).locator(".session-outcome")).toHaveText("applied · real-signed, not stored");
  await expect(step(page, 4)).toContainText("fork_allowRealSigners");
  await step(page, 4).locator(".session-step-head").click();
  await expect(step(page, 4).locator(".session-step-body")).toContainText("refused on mainnet");
  expect((await forkInfo(page)).allowRealSigners).toBe(false);
  await expect(page.locator("#session-replayable")).toBeVisible();
  await expect(page.locator("#session-replayable")).toContainText(real.publicKey());
  await expect(page.locator("#session-verdict")).toContainText("FAILED");

  // Neither the envelope nor the allowance is stored: a reload has only the payer.
  await page.reload();
  await expect(page.locator("#session-progress")).toContainText("Resumed: replayed 1 logged call", { timeout: 60_000 });
  await expect(page.locator("#session-steps li")).toHaveCount(1);
  expect((await forkInfo(page)).allowRealSigners).toBe(false);
});

test("declining in the wallet leaves real signers refused", async ({ page, context }) => {
  test.setTimeout(180_000);
  const real = Keypair.random();
  await recordedRpc(context, TESTNET_RPC, withRealAccount(real).recording);
  await recordedRpc(context, MAINNET_RPC, mainnetWithRealAccount(real).recording);
  await realWallet(page, real, () => {
    throw new Error("the user declined the request");
  });
  await fork(page, "mainnet");
  await reviewWalletCall(page, real);
  await page.locator("#replay-ack").check();
  await page.locator("#confirm-submit").click();
  await expect(page.locator("#submit-err")).toContainText("declined", { timeout: 60_000 });
  await page.locator("#submit-dialog .dialog-close").click();

  // Nothing was allowed: the only safety step refuses real signers again.
  expect((await forkInfo(page)).allowRealSigners).toBe(false);
  await expect(page.locator("#session-steps li")).toHaveCount(2);
  await step(page, 2).locator(".session-step-head").click();
  await expect(step(page, 2).locator(".session-step-body")).toContainText("refused on mainnet");
});

test("a wallet's Testnet envelope keeps its Testnet sequence after a session submit", async ({ page, context }) => {
  test.setTimeout(240_000);
  const real = Keypair.random();
  const { recording, seq } = withRealAccount(real);
  await recordedRpc(context, TESTNET_RPC, recording);
  await realWallet(page, real, (envelope, passphrase) => {
    const tx = TransactionBuilder.fromXdr(envelope, passphrase);
    tx.sign(real);
    return tx.toEnvelope().toXdr("base64");
  });
  await fork(page, "testnet");
  await reviewWalletCall(page, real);
  await page.locator("#replay-ack").check();
  await page.locator("#confirm-submit").click();
  await expect(page.locator("#submit-progress")).toContainText("Applied in the session", { timeout: 60_000 });
  await page.locator("#submit-dialog .dialog-close").click();
  // The session moved the account's sequence on; Testnet did not.
  await expect(step(page, 2)).toHaveAttribute("data-outcome", "applied");

  await reviewWalletCall(page, real);
  const envelope = (await page.locator("#review-xdr").textContent())!;
  const tx = TransactionBuilder.fromXdr(envelope, Networks.TESTNET) as Transaction;
  expect(tx.sequence).toBe((seq + 1n).toString());
});

test("a session open in one tab does not open in another", async ({ page, context }) => {
  test.setTimeout(120_000);
  await recordedRpc(context, TESTNET_RPC, testnetFork());
  await fork(page, "testnet");
  const title = (await page.locator("#session-state").textContent())!;

  // The second tab would reopen it on load; the first tab holds it.
  const other = await context.newPage();
  await other.goto("/");
  await expect(other.locator("#session-err")).toContainText("open in another tab", { timeout: 60_000 });
  await expect(other.locator("#session-state")).toHaveText("no session open");

  // Closed in the first tab, it opens in the second.
  await page.locator("#session-close").click();
  await expect(page.locator("#session-state")).toHaveText("no session open");
  await other.locator("#session-saved").getByRole("button", { name: "resume" }).click();
  await expect(other.locator("#session-state")).toHaveText(title, { timeout: 60_000 });
});

test("an envelope a real key signed is not stored even when the session rejects it", async ({ page, context }) => {
  test.setTimeout(120_000);
  const real = Keypair.random();
  const { recording, seq } = withRealAccount(real);
  await recordedRpc(context, TESTNET_RPC, recording);
  await fork(page, "testnet");
  // A sequence number ahead of the account's: rejected here, valid on the
  // real network once the account catches up.
  const wasm = readFileSync(repo("fixtures/contracts/add_i32.wasm"));
  const ahead = new TransactionBuilder(new Account(real.publicKey(), (seq + 4n).toString()), { fee: "100", networkPassphrase: Networks.TESTNET })
    .addOperation(Operation.uploadContractWasm({ wasm }))
    .setTimeout(0)
    .build();
  ahead.sign(real);
  await page.locator(".session-envelope summary").click();
  await page.locator("#session-envelope").fill(ahead.toEnvelope().toXdr("base64"));
  await page.locator("#session-envelope-submit").click();
  await expect(page.locator("#session-progress")).toContainText("sendTransaction: ERROR", { timeout: 60_000 });

  await expect(step(page, 2)).toHaveAttribute("data-outcome", "rejected");
  await expect(step(page, 2).locator(".session-outcome")).toHaveText("rejected · real-signed, not stored");
  await step(page, 2).locator(".session-step-head").click();
  await expect(step(page, 2).locator(".session-step-body")).toBeVisible();
  await expect(step(page, 2).getByRole("button", { name: "copy envelope XDR" })).toHaveCount(0);
  await expect(page.locator("#session-replayable")).toContainText(real.publicKey());
  await page.reload();
  await expect(page.locator("#session-progress")).toContainText("Resumed: replayed 1 logged call", { timeout: 60_000 });
  await expect(page.locator("#session-steps li")).toHaveCount(1);
});

test("a real wallet cannot sign for a mainnet session whose clock is ahead of real time", async ({ page, context }) => {
  test.setTimeout(180_000);
  const real = Keypair.random();
  await recordedRpc(context, TESTNET_RPC, withRealAccount(real).recording);
  await recordedRpc(context, MAINNET_RPC, closingAt(mainnetWithRealAccount(real).recording, Math.floor(Date.now() / 1000)));
  let asked = 0;
  await realWallet(page, real, (envelope, passphrase) => {
    asked += 1;
    const tx = TransactionBuilder.fromXdr(envelope, passphrase);
    tx.sign(real);
    return tx.toEnvelope().toXdr("base64");
  });
  await fork(page, "mainnet");
  // Ten minutes of ledgers: past what a five-minute envelope covers.
  await page.locator("#session-advance-n").fill("120");
  await page.locator("#session-advance").click();
  await expect(page.locator("#session-steps li")).toHaveCount(2);

  await reviewWalletCall(page, real);
  await page.locator("#replay-ack").check();
  await page.locator("#confirm-submit").click();
  await expect(page.locator("#submit-err")).toContainText("this session's clock is ahead of real time", { timeout: 60_000 });
  await expect(page.locator("#submit-err")).toContainText("use a throwaway key");
  await page.locator("#submit-dialog .dialog-close").click();
  expect(asked).toBe(0);
  await expect(page.locator("#session-steps li")).toHaveCount(2);
  expect((await forkInfo(page)).allowRealSigners).toBe(false);
});
