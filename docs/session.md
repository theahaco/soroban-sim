# Network sessions

A network session rehearses a whole multi-transaction, multi-signer setup on a
fork of a real network before it goes to mainnet ([#23]). Transactions apply in
order over forked state, each one seeing every earlier write. Every envelope
and every authorization entry is really signed and really verified, fees are
charged and refunded, and the clock moves between ledgers. Nothing is ever
submitted: throwaway keys stand in for every real signer, and the RPC client
refuses any method that is not a read.

[#23]: https://github.com/theahaco/soroban-sim/issues/23

## Quick start

```sh
cargo run -p soroban-sim-cli -- session check examples/session/perch-nido-bootstrap.yaml
cargo run -p soroban-sim-cli -- session run examples/session/perch-nido-bootstrap.yaml \
  --wasm-dir path/to/release-wasm --differential --report bootstrap
# -> bootstrap.json and bootstrap.md
```

`examples/session/perch-nido-bootstrap.yaml` is the Perch/Nido registry
bootstrap on mainnet: 37 envelopes, 33 of them applied, plus the negative
controls. The accounts it needs besides the root manager's are created and
funded by real `CreateAccount` operations, and the admin account takes a
co-signer through `SetOptions`, so the only overrides are the root manager's
key substitution and top-up. `crates/soroban-sim-cli/tests/rehearsal.yaml` is a smaller plan that
runs offline over the repository's fixture contracts and uses every step kind
and signer adapter.

`soroban-sim session run` options:

| Option | Meaning |
|---|---|
| `--fork mainnet\|testnet\|URL` | The network to fork (default: the plan's `network`). |
| `--fork-mode head\|checkpoint` | `head` (default) reads RPC's latest ledger lazily; `checkpoint` pins one ledger and rolls every read back to it (see "Fork modes"). |
| `--fork-ledger N` | The ledger a `checkpoint` fork pins (default: the latest when it opens). |
| `--network-passphrase P` | Checked against the RPC's `getNetwork`. |
| `--snapshot FILE` | Fork a snapshot file instead (offline, reproducible). |
| `--save-snapshot FILE` | Save every base entry the run read, for offline replays. |
| `--wasm-dir DIR` | Where `upload` steps find Wasm by hash (repeatable). |
| `--differential` | Compare each prepared step with RPC `simulateTransaction`. |
| `--var NAME=VALUE` | Set a plan variable (repeatable). |
| `--report PREFIX` | Write `PREFIX.json` and `PREFIX.md` (default: `<plan>.report`). |
| `--keep-going` | Run the remaining steps after an unexpected failure. |

The exit code is 0 when every step did what the plan expected, 1 when a step
did not, and 2 on an error before or outside the steps.

## How it works

```text
Session (soroban-sim-core::session)
  prepare():         simulate (recording) -> sign auth entries -> re-simulate
                     (enforcing, so signature checks are metered) -> signed envelope
  prepare_classic(): classic operations -> signed envelope
  submit_ledger():   the transaction set (TxValidator; one per source account,
                     each fee source's bids within its balance, per-ledger
                     limits) -> every fee -> each transaction validated
                     again and applied in core's order -> refunds -> close
  submit():          submit_ledger() with one envelope
  ForkedLedger:      overlay + logged overrides over a lazily fetched base
```

- **ForkedLedger.** Session writes and overrides live in an overlay over a
  read-through cache of the base (RPC `getLedgerEntries`, a snapshot file, or
  fixtures). The cache keeps what the base said, so an archived entry (RPC
  returns evicted entries with `liveUntilLedgerSeq = 0`) stays distinct from an
  absent one. Storage of a contract the session created is answered absent
  locally. Every fetch is logged with the ledger it was served at and the
  entry's last-modified ledger, which is the drift report.
- **TxValidator.** The subset of stellar-core's `TransactionFrame` a
  transaction meets before it applies: time and ledger bounds, minimum
  sequence number, sequence age and gap, extra signers, the sequence number,
  frozen ledger keys (CAP-77), envelope signatures ported from core's
  `SignatureChecker` (the source's low threshold, then each operation's
  threshold, then `txBAD_AUTH_EXTRA` for an unused signature), the fee bid,
  for Soroban the resource fee against the non-refundable fee of the declared
  resources and the network's per-transaction limits, for classic each
  operation's well-formedness (`doCheckValid`), and the source's balance above
  its reserve and liabilities. A rejected envelope pays nothing and reaches no
  ledger.
- **Applier.** The ledger close charges the fee and consumes the sequence
  number first, so a failed operation still pays. The host runs in enforcing
  mode over the
  declared footprint and instruction budget with a session module cache (as
  core keeps every live contract pre-parsed; without it the host charges full
  Wasm parsing and real budgets fail). Archived entries marked for restore get
  core's restored TTL, unmarked ones fail the operation, expired temporary
  entries are absent. Written bytes and events are checked against the
  declared and network limits, rent and events are charged against the
  refundable fee, and the rest is refunded once the ledger's transactions have
  all applied.
- **Clock.** Each submission closes a ledger (a `ledger` block closes one for
  several transactions), 5 s apart by default. `advance` closes many at once
  in O(1); TTLs follow the sequence number, so expiry and archival follow from
  it.

## Classic operations

`CreateAccount`, `Payment` (XLM and issued assets), `SetOptions` (signers,
thresholds, master weight, home domain, flags, inflation destination) and
`ChangeTrust` (open, change, close a trustline) are ports of stellar-core
v29's operation frames: `doCheckValid` before inclusion (a malformed
operation is `txFAILED` with its `…_MALFORMED` code and pays nothing), then
`doApply` against the ledger in core's check order, so the first failing check
decides the result code (`CREATE_ACCOUNT_LOW_RESERVE`, `PAYMENT_UNDERFUNDED`,
`PAYMENT_NO_TRUST`, `SET_OPTIONS_TOO_MANY_SIGNERS`, `CHANGE_TRUST_INVALID_LIMIT`,
`opTOO_MANY_SUBENTRIES`, …). Reserves count sub-entries (signers and
trustlines), sponsorships the fork already holds are released the way core
releases them, and an issuer pays and is paid from its own unlimited line.
Each operation's source must sign at its threshold: high for signer, weight
and threshold changes, medium otherwise. An operation may name an account
created earlier in the same transaction as its source, signed by that key.
Every operation of a transaction runs and reports its result, but a failed
transaction keeps none of their writes, only its fee and sequence number.

Not modelled (the validator refuses them with `txNOT_SUPPORTED`): path
payments, offers and the DEX, liquidity pools, claimable balances, clawback,
creating sponsorships, account merges, manage data and bump sequence.

`crates/soroban-sim-cli/tests/classic_replay.rs` checks the port against the
network: it replays a recorded sample of real mainnet classic transactions
(`fixtures/replay/classic-mainnet.json`), each on a fork of exactly the state
it applied against, and requires the network's result code, every
operation's result, the fee charged and every entry's after-image.

## Several transactions per ledger

`Session::submit_ledger` (a plan's `ledger` block, the facade's
`fork_holdLedger`) closes one ledger the way core closes one:

1. **The set.** Each envelope is validated against the ledger as it was
   before; a refused one gets its result code. Core's queue holds one
   transaction per source account, so a second one waits for the next ledger.
   As core's queue does, the fee bids are totalled per fee source in submission
   order, and the transaction that takes its fee source past its balance above
   the reserve is refused with `txINSUFFICIENT_BALANCE`. The per-ledger limits come from the ledger: the header's `maxTxSetSize`
   (classic operations; a snapshot saved from an RPC fork keeps it, and one
   without it has no classic limit) and the `ConfigSetting` entries for the Soroban phase
   (transactions, instructions, transaction bytes, disk reads and writes). The
   highest inclusion fee rate goes first; what does not fit waits for the next
   ledger, and what no ledger can hold is refused.
2. **Apply order.** The classic phase, then the Soroban phase, each shuffled by
   the transaction's full hash XOR the transaction-set hash, as core orders
   them.
3. **Fees first.** Every fee is charged before any transaction applies, so an
   earlier transaction cannot spend a later one's fee.
4. **Apply.** Each transaction is validated again against the ledger as the
   ones before it left it (core's apply-time checks: a source whose threshold
   changed meanwhile fails with `txFAILED`/`opBAD_AUTH` and still pays), its
   sequence number consumed and a pre-auth signer for it removed, then its
   operations applied. Soroban transactions get core's PRNG seed,
   `sha256(txSetHash ‖ applyIndex)`, over the session's own transaction set.
5. **Refunds** once every transaction has applied, then the ledger closes.

An error (an RPC fetch failing, say) leaves the session as it was before the
call, even when an earlier ledger of the batch had already closed.

## Signers

Keys are throwaway: `sha256("soroban-sim throwaway:" || label)` seeds the key.
A G account keeps its address, thresholds and signer weights; its real master
key's weight moves to a throwaway signer and the master drops to 0
(`substitute_master_key`), or a real signer is replaced by a throwaway of the
same weight (`substitute_signer`). The account's sub-entry count is kept, so its
reserve matches the real account. Each substitution is an override in the
report.

Authorization-entry signer adapters:

| Plan | What it produces |
|---|---|
| `classic: [key, …]` | A G account's `Vec<AccountEd25519Signature>`, verified by the host against the account's signers and medium threshold. |
| `classic: { keys, address_bound: true }` | The same as CAP-71 `AddressV2` credentials (the payload binds the address). |
| `oz: { digest: oz_rule_ids, … }` | An OpenZeppelin smart account's `AuthPayload`, `External` signers signing `sha256(payload ‖ rule ids XDR)`. |
| `oz: { digest: addr_oz_v1, … }` | The same, with the payload first bound to the account: `sha256("perch/account/auth-addr-v1" ‖ contract id ‖ payload)` (`domain:` overrides the tag). |
| `external: { verifier, key }` with an ed25519 key | 64-byte ed25519 signature over the digest. |
| … with a `secp256r1` key | ECDSA P-256 over `sha256(digest)`, low-s. |
| … with a `webauthn` key | A `webauthn.get` assertion whose challenge is the digest, as OZ's `WebAuthnSigData` XDR; `key_data` is the 65-byte key plus the credential id. |
| `delegated: { account: G…, keys }` | CAP-71: the account names `Delegated(G)` and the G's keys sign the address-bound payload in an `AddressWithDelegates` entry. |

`rule:` is an id, a list of ids, or `{ name: … }`, looked up with the account's
`get_context_rule(id)` when the step signs (rule ids move when rules change).

## Plan reference

A plan is YAML or JSON. Strings anywhere use `${name}` interpolation over plan
`vars` (evaluated lazily, in any order), values saved by steps, `--var`
values, `key.NAME` (the key's G address), `key.NAME.pk` (its public key, hex),
`ledger` and `time` (the ledger the next transaction applies in and its close
time) and `network.passphrase`.

```yaml
name: …
description: …
network: mainnet               # mainnet | testnet | { rpc: URL, passphrase: … }
options:
  source: "${theahaco}"        # default envelope source
  seconds_per_ledger: 5
  auth_expiration_ledgers: 100
  wasm_dir: wasm               # relative to the plan
  bytes_source: testnet        # fetch Wasm by hash, read-only
keys:
  theahaco: ed25519            # label = name
  admin: { ed25519: author-admin }
  hsm: secp256r1
  passkey: { webauthn: passkey, credential_id: "6372656431" }
vars:
  root: { contract_id: { deployer: "${theahaco}", salt: { sha256: "2026-04-24T13:41:00" } } }
  version: { view: { contract: "${root}", fn: current_version, args: [{ string: registry }] } }
  expiry: { ledger_plus: 40 }
signers:                       # auth-entry signers by address, for every step
  "${theahaco}": { classic: [theahaco] }
setup:                         # overrides before the first step
  - substitute_master_key: { account: "${theahaco}", key: theahaco }
  - fund: { account: "${theahaco}", xlm: 500 }
steps: [ … ]
```

Values (`vars`, `set`, `salt`, `to_ledger`, `xlm`): text, a number,
`{ contract_id: { deployer, salt } }`, `{ sha256: text }` (hex),
`{ ledger_plus: N }`, or `{ view: { contract, fn, args } }` (the rendered result).

Arguments are stellar-xdr's ScVal JSON (`{ string: … }`, `{ address: … }`,
`{ u32: 1 }`, `{ i128: "1" }`, `{ bytes: hex }`, `{ symbol: … }`,
`{ vec: [ … ] }`, `void`, …), plus `{ bytes_utf8: text }` and
`{ bytes_sha256: text }`. Interpolated numbers may be strings (`{ u32: "${n}" }`).

Overrides (`setup` and `override` steps), each logged with its `label`:
`substitute_master_key: { account, key }`,
`substitute_signer: { account, signer: G…, key }`,
`fund: { account, xlm | stroops }` (creates the account if needed),
`set_entry: { xdr: LedgerEntry base64, live_until }`,
`remove_entry: LedgerKey base64`.

Steps have a `name` and exactly one action:

| Action | Meaning |
|---|---|
| `invoke: { contract, fn, args }` | A contract call. |
| `upload: { file \| hash, skip_if_present }` | Upload Wasm; by default skipped when the code is already on the ledger, live or archived. |
| `deploy: { deployer, salt, wasm_hash, args }` | `CreateContractV2` with constructor arguments. |
| `deploy_sac: native` | The native asset's Stellar Asset Contract. |
| `envelope: base64` | Submit a signed envelope as is. |
| `create_account: { destination, starting_balance }` | A classic transaction with one `CreateAccount` (XLM, up to 7 decimals). |
| `payment: { destination, asset, amount }` | `Payment`; `asset` is `native` (the default) or `CODE:ISSUER`. |
| `set_options: { signer: { key, weight }, master_weight, low_threshold, med_threshold, high_threshold, home_domain, set_flags, clear_flags, inflation_dest }` | `SetOptions`; `key` is a plan key or a G address, weight 0 removes the signer; flags by name (`auth_required`, `auth_revocable`, `auth_immutable`, `auth_clawback_enabled`) or bits. |
| `change_trust: { asset: CODE:ISSUER, limit }` | `ChangeTrust`; no `limit` is the maximum, 0 closes the line. |
| `operations: [ … ]` | One classic transaction of several operations, each one of the four above with an optional `source`. |
| `ledger: [ steps ]` | Transaction steps submitted for the same ledger (see above). Each is prepared against the state before the ledger, sequence numbers chained per source, and reported on its own row. |
| `view: { contract, fn, args }` | A read-only call; `fresh_fork: true` runs it on a fork without the session's writes and overrides. |
| `advance: { ledgers, seconds } \| { to_ledger } \| { to_time } \| { seconds }` | Close ledgers. |
| `set: { name: value }` | Set variables now (for example `{ ledger_plus: 40 }`). |
| `assert: [{ value, equals }]` | Compare values (derived addresses against known ones). |
| `override: { … }` | One override mid-session. |

Transaction steps also take `source`, `sign_with: [keys]` (default: the
classic signers registered for the source, else the key whose account is the
source; a classic step adds the keys of its operations' sources),
`fee_bump: { source, fee, sign_with }` (another account pays the fee in a fee
bump; `fee` defaults to the inner transaction's inclusion rate over one more
operation, and the fee source's signers resolve like a source's), `auth: { address: signer }` (over the plan's `signers`),
`auth_expiration_ledgers`, `save_envelope: name`, and `hold: true` (prepare and
sign but do not submit; submit it later with `envelope: "${name}"`).

Every step may `save: name` its rendered result and set `expect`:
`{ equals: text }`, `{ value: ScVal JSON }`, or `{ fails: true | "text" }` (the
step must not apply, and the outcome must mention the text: `txBAD_AUTH`,
`txBAD_SEQ`, `3002`, `opBAD_AUTH`, `PAYMENT_UNDERFUNDED`, …). A classic
step's outcome names every operation's result code. A step without `expect`
must succeed, and the run stops at the first one that does not.

```yaml
steps:
  - name: theahaco creates the admin's and the CI delegate's accounts
    operations:
      - create_account: { destination: "${key.admin}", starting_balance: 10 }
      - create_account: { destination: "${key.ci}", starting_balance: 5 }
  - name: one ledger, two sources
    ledger:
      - name: the admin account adds a co-signer
        source: "${key.admin}"
        set_options: { signer: { key: admin_backup, weight: 1 }, low_threshold: 1, med_threshold: 2, high_threshold: 2 }
      - name: create-registry perch
        invoke: { contract: "${root}", fn: deploy, args: [ … ] }
  - name: "negative: the admin's key alone"
    payment: { destination: "${theahaco}", amount: "0.0000001" }
    source: "${key.admin}"
    sign_with: [admin]
    expect: { fails: opBAD_AUTH }
```

## The report

`PREFIX.json` (format `soroban-sim-session-report/1`) and `PREFIX.md` hold:
the fork point and host version; per-step outcome, fees (bid, resource,
non-refundable, refundable, charged, refunded, rent, events), CPU used against
declared, writes, restores, contract errors, the matched signers, and for
classic transactions every operation's result code, the position in the
ledger's apply order, why it waited for a later ledger, and the code of a
transaction refused by the checks core repeats at apply; every override; fees
by payer with the largest fee bid each must cover; account balances and
sequence numbers before and after, with every other change (signers added,
reweighed or removed, thresholds, home domain, flags, sub-entries); trustline
balances and limits; the final diff against the fork point; drift (the
ledgers fetches were served at and any entry modified after the fork ledger);
fetch counts; the differential results; and the safety section.

## Differential check

`--differential` compares each prepared step's recording pass with RPC
`simulateTransaction` of the same unsigned transaction: return value,
footprint, instructions, disk-read and write bytes, resource fee, and auth
invocations. A step that reads anything the session changed cannot be
simulated by the real network and is reported as skipped. On the bootstrap,
every comparable upload matches exactly and `root.deploy` matches except for a
1-stroop resource-fee difference.

## Safety

The session never submits. Rehearsal envelopes keep the real network id, so an
envelope signed by a real key would be valid on the real network. The
validator flags every envelope signature, and every G-account auth-entry
signature (including CAP-71 delegates), that verifies against a signer the
base ledger already has; on mainnet such a signature fails the run.
Smart-account `External` and passkey signatures are not checked yet.

## Fork modes

- **`head`** (default). Entries are fetched lazily from RPC `getLedgerEntries`,
  which only serves its latest ledger, so a session that runs for minutes can
  mix ledgers. The drift report lists every fetched entry modified after the
  fork ledger.
- **`checkpoint`**. One consistent ledger. The fork pins ledger N (`--fork-ledger`,
  default the latest), and every read is rolled back to it: RPC answers at its
  latest ledger S, the fork reads the close meta of ledgers N+1..S with
  `getLedgers`, and each entry takes the before-image of its first change after
  N (entries nothing changed are already as they were). Answers are cached, so
  every fresh fork of the session sees the same state. Two limits: RPC must
  still serve the ledgers since N (at most 720 behind, about an hour; each
  mainnet ledger's meta is a few MB), and N must be no older than the last
  change of the Soroban state-size window that sets the rent rate. Core
  rewrites that window every 64 ledgers at ledger close, outside any
  transaction's meta, so nothing records its older values; the fork refuses
  an older pin and names the first ledger it accepts. An eviction records no
  before-image either: a read of an entry (or its TTL) evicted after N, with
  no earlier change since N recording its value, fails and names the key and
  the eviction ledger instead of serving it as absent.

## Fidelity and limits

The host is exact: contracts run in the same `soroban-env-host` release as the
network's protocol (the session refuses any other), and transactions apply with
a build of it that is metered exactly as stellar-core's (simulation needs the
`recording_mode` feature, and a recording-mode build charges an extra
storage-map lookup on every write, about 0.2% more CPU; the applier uses a
second build of the same release without it). The transaction layer is a port
of core's `TransactionFrame` and `FeeBumpTransactionFrame` checks, in core's
order, and is checked against real network transactions: `soroban-sim replay`
replays samples of recent mainnet and testnet Soroban transactions, fee bumps
and smart-account `__check_auth` included, and compares every result, return
value, event, after-image, fee and core resource metric
([`docs/replay.md`](replay.md)); `tests/classic_replay.rs` does the same for
classic transactions. The committed samples match exactly.

Modelled: v1 and fee-bump envelopes (`txFEE_BUMP_INNER_FAILED` for an inner
rejection), Soroban or classic inside, muxed sources, preconditions, sequence
numbers, frozen ledger keys (CAP-77), signatures and thresholds,
pre-authorized-transaction signers removed after use, the inclusion fee capped
at the lane base fee (`SubmitOptions::lane_base_fee`, for a surge-priced
ledger), the resource fee and per-transaction limits, footprint key and entry
size limits, disk-read metering of classic and restored entries, restores of
marked archived entries only, rent and events against the refundable fee,
refunds to the fee source, the classic operations above, and ledgers of
several transactions with core's per-ledger limits and apply order. Each
transaction gets core's PRNG seed, `sha256(txSetContentsHash || applyIndex)`,
over the session's own transaction set (`SubmitOptions::prng_seed` sets the
real one, for a replay).

Not modelled yet: `ExtendFootprintTtl` and `RestoreFootprint` operations (an
`InvokeHostFunction` restores marked entries itself), the other classic
operations, surge pricing beyond a given lane base fee, and core's parallel
Soroban clusters (a ledger's Soroban phase is one cluster). In a parallel
Soroban phase core bumps every transaction's sequence number before any
executes; the session bumps each as it applies, so a Soroban transaction whose
footprint holds another transaction's source account sees that bump only if
the other applied first.

The same session also runs behind an RPC facade, `soroban-sim serve --fork …`,
so existing scripts and stock tools rehearse themselves unchanged
([`docs/rpc-facade.md`](rpc-facade.md)), and in the browser, where the web
demo serves it to stellar-sdk at a virtual RPC URL
([`docs/browser-session.md`](browser-session.md)).
