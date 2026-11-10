# Browser network sessions

The web demo runs a network session ([`docs/session.md`](session.md)) in the
browser: a fork of Testnet or Mainnet that applies signed transactions in
order, each seeing every earlier write. Every envelope and authorization
signature is verified, fees are charged and refunded, and the clock moves.
Nothing is ever submitted to a network ([#23], phase 4).

The session answers the same stellar-rpc and `fork_*` JSON-RPC methods as
`soroban-sim serve` ([`docs/rpc-facade.md`](rpc-facade.md)), at a virtual RPC
URL that only this browser serves. So stellar-sdk code, including the demo's
own wallet pre-submit flow, submits into the fork unchanged.

[#23]: https://github.com/theahaco/soroban-sim/issues/23

## Using it

In the **Network session** panel:

1. Pick Testnet, Mainnet or a custom RPC URL and **Fork at the latest ledger**.
   The fork reads the real network lazily over RPC, read-only. The session
   starts with a throwaway fee payer, funded by an override.
2. Put transactions in, in any of these ways:
   - **The workbench.** While a session is open, the local identity is the
     session's throwaway payer. Preflight a contract call as usual, open
     **Review sign & submit**, and pick **Into the open network session**.
     The call is prepared again against the session's state with
     `prepareTransaction`, signed, and sent with `sendTransaction` to the
     virtual RPC.
   - **Import a plan.** A `soroban-sim session` plan file, together with the
     Wasm files its upload steps name. See "Plans in the browser" below.
   - **Submit a signed envelope.** Paste base64 envelope XDR.
   - **Your own code.** Point `new rpc.Server(url)` at the URL the panel
     shows as **Virtual RPC**, from a page of the same origin. Add
     `{ allowHttp: true }` on `http://localhost`.
3. Read the **step timeline**. Each step shows its fees (bid, resource,
   refundable, charged, refunded, rent), CPU and write bytes against what was
   declared, the envelope signatures that matched, the authorization entries,
   and every ledger entry it changed, before and after. **Open in time-travel
   debugger** traces any transaction step in the workbench's debugger.
4. Close ledgers, take a snapshot and revert to it, then read the **report**.
   It is `soroban-sim session run`'s report (`soroban-sim-session-report/1`),
   with every override, fees by payer, the final diff and drift. Download it
   as JSON or Markdown.

A reload reopens the session that was open. Stored sessions are listed under
the fork controls, where you can resume or delete them.

## How it works

```text
page (main.ts, session-ui.ts)
 │  stellar-sdk ──fetch <base>/__soroban-sim/rpc/<id>──▶ service worker (public/sw.js)
 │                                                          │ "soroban-sim/rpc" + MessagePort
 │◀─────────────────────────────────────────────────────────┘
 │  relays the body and the port
 ▼
session worker (session-worker.ts)
  PersistentSession ── IndexedDB: sessions / ops (the log) / base (the fork cache)
   └ SorobanSession ── needs-state loop: fetch base entries over read-only RPC, provide, repeat
      └ component: WIT network-session resource
         └ soroban-sim-facade::Facade ── the JSON-RPC methods `soroban-sim serve` answers
            └ soroban-sim-core::session::Session ── validator, applier, forked ledger
```

- **One request handler.** `crates/soroban-sim-facade` holds the stellar-rpc
  and `fork_*` methods without a transport. `soroban-sim serve` wraps it in
  HTTP. The component exports it as the WIT `network-session` resource
  (`wit/simulator.wit`): `rpc(body)` for any JSON-RPC body, plus typed
  methods for the main operations (`submit`, `advance`, `set-time`,
  `apply-override`, `snapshot`, `revert`, `allow-real-signers`, `steps`,
  `debug-context`, `report`).
- **The base arrives between calls.** A synchronous Wasm call cannot wait
  for `fetch`. A call that reads a base entry the caller has not provided yet
  is undone: the session's writes, overrides, clock and log, and the facade's
  steps, stored transactions and snapshots all go back to where they were.
  The call returns `needs-state` with the keys. `SorobanSession` fetches them
  with `getLedgerEntries` (`ReadOnlyRpc` can call only read methods, and backs
  off on 429), `provide`s them, and repeats the same call. Opening a session
  asks for all sixteen network settings at once, and a `sendTransaction`
  asks for every key its envelope names (source, auth accounts, footprint,
  what its classic operations touch) in one round. Simulations discover keys as they run, like `simulate` does.
- **A session is a log.** Every JSON-RPC body that changed the session is
  appended to IndexedDB after the session answered it. So is every base batch
  the session read. Resuming forks the same ledger again, hands the component
  the whole cache, and replays the log. The component is deterministic and
  envelopes carry their signatures, so the same log gives the same session
  without a network read. Nothing secret is stored: throwaway keys derive from
  labels, and envelopes are public by design. An envelope a real key signed is
  never logged, whether the session applied, rejected or refused it, because the real network would accept
  it. The timeline marks its step "real-signed, not stored" and offers no
  copy of it, and a resumed session does not have it. `fork_allowRealSigners`
  is not logged either, so a resumed session refuses real signers again.
- **The service worker is a relay.** It holds no state, so the browser can
  stop it whenever it likes. It asks the tab that sent the request first,
  then any other tab of the app. The virtual RPC answers only while a tab
  holds that session open. Service workers need a secure context:
  `http://localhost`, `http://127.0.0.1` or `https`.
- **Calls run one at a time.** A session is one ordered history, so a call
  waiting on the network does not let a later one jump the queue.

The TypeScript API is in `packages/soroban-sim/src/session/`:
`SorobanSession` (the resource and the needs-state loop), `RpcBaseLedger` and
`ReadOnlyRpc` (the base), `forkConfigFromRpc` (the fork point),
`SessionStore` and `PersistentSession` (IndexedDB), and `throwawayKeypair`
(the same derivation as `Ed25519Key::throwaway`). A page imports
`@theahaco/soroban-sim/browser`, which does not load the component.

## Signing and replay safety

The session keeps the real network passphrase, so every signature it checks
is a real one. An envelope that a key the real network trusts has signed is
valid on that network as it stands.

- **Throwaway keys by default.** The workbench signs with the session's
  throwaway payer. Plan keys are throwaway keys by label. Neither is known to
  any real network.
- **A real wallet gets a replay warning.** If you pick **Connect wallet** and
  submit into a session, the review dialog says that the wallet's signature is
  valid on the real network. The button stays disabled until you acknowledge
  it. On a Mainnet fork the warning is about Mainnet and spending from your
  real account. The acknowledgement covers that one envelope: the session
  allows real signers (`fork_allowRealSigners`) only once the wallet has
  signed, and refuses them again after the send, even if the wallet declined.
  That envelope expires five minutes after it is built, so the dialog refuses
  to ask the wallet at all while the session's clock (the next ledger's close
  time) is more than five minutes ahead of real time, for example after
  advancing ledgers. Use a throwaway key there. The report records both
  safety steps.
- **Mainnet refuses real signers otherwise.** Like `soroban-sim serve`, a
  Mainnet session refuses an envelope that a signer of the base ledger signed,
  with JSON-RPC error `-32001`. The timeline shows the step as refused. The
  report lists the signature under Safety and fails. On other networks the
  envelope is applied and still listed.
- A Mainnet fork shows a standing warning in the session panel.

## Plans in the browser

**Import a plan** runs a plan file ([plan reference](session.md#plan-reference))
as stellar-sdk code against the virtual RPC. Each transaction step is built,
simulated, signed (authorization entries, then a second simulation so the
signature checks are metered, then the envelope) and sent, the way a dapp
sends it to a real RPC. Overrides and clock moves are `fork_*` calls.

The browser runs plans that need no native signer adapter:

- `ed25519` keys (throwaway, by label: the same addresses as the CLI);
- `classic` signers, single or multi-key;
- every contract, clock and override step kind except `view` with
  `fresh_fork` (classic-operation and `ledger` steps run in the CLI only);
- `vars` with `sha256`, `contract_id`, `ledger_plus` and `view`; `${…}`
  interpolation; `expect` (`equals`, `value`, `fails`) and `save`;
- `upload` by attached `file` (matched by file name) or by `hash`.

Plans with OpenZeppelin, external (secp256r1, WebAuthn), delegated or
address-bound classic signers, or with `create_account`, `payment`,
`set_options`, `change_trust`, `operations` or `ledger` steps, are refused at
import, with a pointer to `soroban-sim session run`. `examples/session/two-step-add.yaml` runs in both:
the CLI test and the browser test derive the same contract from it over the
same recorded fork.

## Tests

- `crates/soroban-sim-facade/tests/facade.rs`: the facade over a deferred
  base, through JSON-RPC only. It covers a dependent upload, deploy and call,
  undo and retry, batches, the Mainnet refusal, and revert.
- `crates/soroban-sim-component/src/session.rs`: the WIT resource mapping.
- `packages/soroban-sim/src/session/session.test.ts`: the needs-state loop,
  call ordering, the RPC client's allowlist and backoff, the base ledger, the
  header parser, IndexedDB and replay after a reload.
- `packages/web-demo/tests/session.spec.ts` (Playwright) answers the fork's
  RPC from a recording of Testnet (`tests/fixtures/testnet-fork.json`,
  captured by `scripts/capture-session-fixture.mjs`). It checks the timeline
  and the two-step dependent plan, the debugger, replay after a reload, the
  wallet pre-submit flow into the session, and the replay warning when a real
  signer signs on a Mainnet fork. Nothing in CI reaches a network.

## Limits

These come on top of the session's own limits ([fidelity and
limits](session.md#fidelity-and-limits)) and the facade's ([fidelity and
limits](rpc-facade.md#fidelity-and-limits)):

- Transactions apply with the recording-mode host the component already links,
  not the second, exact-metering build the CLI uses, so CPU on a transaction
  that writes comes out slightly above stellar-core's (about 0.1 to 0.3%; see
  [`docs/replay.md`](replay.md)). The report's fidelity notes say so.
- The workbench's preflight still runs against live Testnet state. A call to a
  contract that exists only in the session cannot be preflighted there. Send
  it from a plan, an envelope or your own stellar-sdk code instead.
- A step whose authorization uses address credentials replays in the debugger
  as a call tree only. The trace pass records auth, and recorded address
  signatures are blank (see `crates/soroban-sim-core/src/trace.rs`).
- One session is open per tab, and one tab per session: a Web Lock keeps a
  session another tab holds from opening (or reopening on reload) here. The
  virtual RPC answers only while that tab is open.
- Wallets that relay their own transactions (Nido's C-account flow) cannot
  submit into a session.
