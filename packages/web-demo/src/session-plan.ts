/**
 * Runs a `soroban-sim session` plan (docs/session.md) in the browser, as
 * ordinary stellar-sdk code against the session's virtual RPC URL: every
 * transaction is built, simulated, signed and sent the way a dapp would send
 * it to a real RPC server, and lands in the session through the service
 * worker. Overrides and clock moves are facade calls.
 *
 * The browser runs the plan subset that needs no native signer adapters:
 * ed25519 throwaway keys (the same labels give the same addresses as the
 * CLI), classic signers, and every contract, clock and override step except
 * `view.fresh_fork`. Plans that need OpenZeppelin, external (secp256r1,
 * WebAuthn) or delegated signers, or that have classic-operation or `ledger`
 * steps, are refused with a pointer to `soroban-sim session run`.
 */
import {
  Account,
  Address,
  Asset,
  authorizeEntry,
  hash,
  Keypair,
  nativeToScVal,
  Operation,
  rpc,
  scValToNative,
  StrKey,
  type Transaction,
  TransactionBuilder,
  xdr,
} from "@stellar/stellar-sdk";
import { throwawayKeypair } from "@theahaco/soroban-sim/browser";
import { parse as parseYaml } from "yaml";

export interface PlanContext {
  /** The session's virtual RPC URL. */
  rpcUrl: string;
  passphrase: string;
  /** Wasm files the user attached, by file name. */
  wasm: Map<string, Uint8Array>;
  /** A facade call (logged for replay): overrides and the clock. */
  call(method: string, params: unknown): Promise<unknown>;
  onProgress(line: string): void;
}

export interface PlanResult {
  name: string;
  steps: number;
  passed: boolean;
  failures: string[];
}

type Json = null | boolean | number | string | Json[] | { [k: string]: Json };
type Obj = { [k: string]: Json };

interface Outcome {
  succeeded: boolean;
  /** applied, failed, rejected, refused, held, skipped, ok */
  outcome: string;
  /** What an `expect.fails` substring is matched against. */
  text: string;
  value?: xdr.ScVal;
}

const SUPPORTED_KEYS = new Set(["name", "description", "network", "options", "keys", "vars", "signers", "setup", "steps"]);
const STEP_ACTIONS = ["invoke", "upload", "deploy", "deploy_sac", "envelope", "view", "advance", "set", "assert", "override"] as const;
/** Plan steps only `soroban-sim session run` runs. */
const CLI_ONLY_STEPS = ["create_account", "payment", "set_options", "change_trust", "operations", "ledger"] as const;

class PlanError extends Error {}

function isObj(v: Json | undefined): v is Obj {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function hex(bytes: Uint8Array): string {
  return [...bytes].map((b) => b.toString(16).padStart(2, "0")).join("");
}

function unhex(text: string, what: string): Uint8Array {
  const t = text.trim();
  if (!/^([0-9a-f]{2})*$/i.test(t)) throw new PlanError(`${what}: ${JSON.stringify(text)} is not hex`);
  return Uint8Array.from(t.match(/../g) ?? [], (b) => parseInt(b, 16));
}

function sha256(bytes: Uint8Array): Uint8Array {
  return new Uint8Array(hash(bytes as unknown as Parameters<typeof hash>[0]));
}

const utf8 = (s: string) => new TextEncoder().encode(s);

/** The text a saved value or `expect.equals` compares (the CLI's `render`). */
function render(v: xdr.ScVal): string {
  const native = scValToNative(v) as unknown;
  if (native === null || native === undefined) return "";
  if (typeof native === "string" || typeof native === "number" || typeof native === "bigint" || typeof native === "boolean") {
    return String(native);
  }
  if (native instanceof Uint8Array) return hex(native);
  return JSON.stringify(native, (_k, x: unknown) => (typeof x === "bigint" ? x.toString() : x));
}

export async function runPlan(text: string, ctx: PlanContext): Promise<PlanResult> {
  const parsed = parseYaml(text) as Json;
  if (!isObj(parsed)) throw new PlanError("a plan is a YAML or JSON object");
  return new Runner(parsed, ctx).run();
}

class Runner {
  private readonly server: rpc.Server;
  private readonly keys = new Map<string, Keypair>();
  private readonly values = new Map<string, string>();
  private readonly evaluating = new Set<string>();
  private readonly vars: Obj;
  private readonly options: Obj;
  private readonly signers: Obj;
  private readonly failures: string[] = [];
  private readonly name: string;

  constructor(
    private readonly plan: Obj,
    private readonly ctx: PlanContext,
  ) {
    this.server = new rpc.Server(ctx.rpcUrl, { allowHttp: ctx.rpcUrl.startsWith("http:") });
    for (const k of Object.keys(plan)) {
      if (!SUPPORTED_KEYS.has(k)) throw new PlanError(`unknown plan field ${k}`);
    }
    this.name = typeof plan.name === "string" ? plan.name : "plan";
    this.options = isObj(plan.options) ? plan.options : {};
    this.vars = isObj(plan.vars) ? plan.vars : {};
    this.signers = isObj(plan.signers) ? plan.signers : {};
    for (const [name, spec] of Object.entries(isObj(plan.keys) ? plan.keys : {})) {
      this.keys.set(name, keyFor(name, spec));
    }
    for (const [address, spec] of Object.entries(this.signers)) checkSigner(address, spec);
    const steps = Array.isArray(plan.steps) ? plan.steps : [];
    for (const [i, step] of steps.entries()) {
      const kind = isObj(step) ? CLI_ONLY_STEPS.find((k) => k in step) : undefined;
      if (kind) throw new PlanError(`step ${i + 1}: ${kind} steps need \`soroban-sim session run\``);
    }
  }

  async run(): Promise<PlanResult> {
    const setup = Array.isArray(this.plan.setup) ? this.plan.setup : [];
    for (const [i, o] of setup.entries()) {
      if (!isObj(o)) throw new PlanError(`setup #${i + 1} is not an override`);
      await this.override(o, `setup #${i + 1}`);
    }
    const steps = Array.isArray(this.plan.steps) ? this.plan.steps : [];
    let ran = 0;
    for (const [i, step] of steps.entries()) {
      if (!isObj(step)) throw new PlanError(`step ${i + 1} is not an object`);
      const label = typeof step.name === "string" ? step.name : `step ${i + 1}`;
      this.ctx.onProgress(`plan "${this.name}": step ${i + 1}/${steps.length} · ${label}`);
      let o: Outcome;
      try {
        o = await this.step(step, label);
      } catch (error) {
        if (error instanceof PlanError) throw error;
        const text = error instanceof Error ? error.message : String(error);
        o = { succeeded: false, outcome: "error", text };
      }
      ran += 1;
      const why = await this.judge(step, o);
      if (why) {
        this.failures.push(`step ${i + 1} (${label}): ${why}`);
        break;
      }
    }
    return { name: this.name, steps: ran, passed: this.failures.length === 0, failures: this.failures };
  }

  // --- values ---

  private async lookup(name: string): Promise<string> {
    const saved = this.values.get(name);
    if (saved !== undefined) return saved;
    if (name === "ledger" || name === "time") {
      const info = (await this.ctx.call("fork_info", {})) as { next: { ledger: number; closeTime: number } };
      return String(name === "ledger" ? info.next.ledger : info.next.closeTime);
    }
    if (name === "network.passphrase") return this.ctx.passphrase;
    if (name.startsWith("key.")) {
      const [key, field = "g"] = name.slice(4).split(".");
      const k = this.key(key!);
      if (field === "g") return k.publicKey();
      if (field === "pk") return hex(k.rawPublicKey());
      throw new PlanError(`key ${key} has no field ${field} (ed25519 keys have g and pk)`);
    }
    if (!(name in this.vars)) throw new PlanError(`unknown variable ${name}`);
    if (this.evaluating.has(name)) throw new PlanError(`variable ${name} refers to itself`);
    this.evaluating.add(name);
    try {
      const v = await this.eval(this.vars[name]!);
      this.values.set(name, v);
      return v;
    } finally {
      this.evaluating.delete(name);
    }
  }

  private async interp(text: string): Promise<string> {
    let out = "";
    let at = 0;
    for (const m of text.matchAll(/\$\{([^}]+)\}/g)) {
      out += text.slice(at, m.index) + (await this.lookup(m[1]!.trim()));
      at = m.index! + m[0].length;
    }
    return out + text.slice(at);
  }

  private async eval(e: Json): Promise<string> {
    if (typeof e === "string") return this.interp(e);
    if (typeof e === "number") return String(e);
    if (isObj(e)) {
      if ("sha256" in e) return hex(sha256(utf8(await this.eval(e.sha256!))));
      if ("ledger_plus" in e) return String(Number(await this.lookup("ledger")) + Number(e.ledger_plus));
      if (isObj(e.contract_id)) {
        const deployer = await this.eval(e.contract_id.deployer!);
        const salt = unhex(await this.eval(e.contract_id.salt!), "contract_id salt");
        return contractId(this.ctx.passphrase, deployer, salt);
      }
      if (isObj(e.view)) {
        const o = await this.simulateCall(e.view);
        if (!o.succeeded || !o.value) throw new PlanError(`view ${JSON.stringify(e.view)}: ${o.text}`);
        return render(o.value);
      }
    }
    throw new PlanError(`unsupported value ${JSON.stringify(e)}`);
  }

  private key(name: string): Keypair {
    const k = this.keys.get(name);
    if (!k) throw new PlanError(`unknown key ${name}`);
    return k;
  }

  /** ScVal JSON (stellar-xdr's serde shape) plus the plan's byte shorthands. */
  private async scval(v: Json): Promise<xdr.ScVal> {
    if (v === "void") return xdr.ScVal.scvVoid();
    if (!isObj(v) || Object.keys(v).length !== 1) throw new PlanError(`${JSON.stringify(v)} is not ScVal JSON`);
    const [kind, inner] = Object.entries(v)[0]!;
    const text = async () => (typeof inner === "string" ? this.interp(inner) : String(inner));
    switch (kind) {
      case "string":
        return xdr.ScVal.scvString(await text());
      case "symbol":
        return xdr.ScVal.scvSymbol(await text());
      case "bool":
        return xdr.ScVal.scvBool(typeof inner === "boolean" ? inner : (await text()) === "true");
      case "u32":
        return xdr.ScVal.scvU32(Number(await text()));
      case "i32":
        return xdr.ScVal.scvI32(Number(await text()));
      case "u64":
      case "i64":
      case "u128":
      case "i128":
      case "u256":
      case "i256":
        return nativeToScVal(BigInt(await text()), { type: kind });
      case "address":
        return new Address(await text()).toScVal();
      case "bytes":
        return xdr.ScVal.scvBytes(unhex(await text(), "bytes") as unknown as Buffer);
      case "bytes_utf8":
        return xdr.ScVal.scvBytes(utf8(await text()) as unknown as Buffer);
      case "bytes_sha256":
        return xdr.ScVal.scvBytes(sha256(utf8(await text())) as unknown as Buffer);
      case "vec":
        if (!Array.isArray(inner)) throw new PlanError("vec takes a list");
        return xdr.ScVal.scvVec(await Promise.all(inner.map((i) => this.scval(i))));
      case "map":
        if (!Array.isArray(inner)) throw new PlanError("map takes a list of { key, val }");
        return xdr.ScVal.scvMap(
          await Promise.all(
            inner.map(async (e) => {
              if (!isObj(e)) throw new PlanError("map entries are { key, val }");
              return new xdr.ScMapEntry({ key: await this.scval(e.key!), val: await this.scval(e.val!) });
            }),
          ),
        );
      default:
        throw new PlanError(`ScVal kind ${kind} is not supported in the browser runner`);
    }
  }

  // --- steps ---

  private async step(step: Obj, label: string): Promise<Outcome> {
    const actions = STEP_ACTIONS.filter((a) => a in step);
    if (actions.length !== 1) throw new PlanError(`${label}: give exactly one of ${STEP_ACTIONS.join(", ")}`);
    const action = actions[0]!;
    switch (action) {
      case "view": {
        const call = step.view as Obj;
        if (call.fresh_fork) throw new PlanError(`${label}: view.fresh_fork is not supported in the browser runner`);
        return this.simulateCall(call);
      }
      case "advance":
        return this.advance(step.advance as Obj);
      case "set": {
        const done: string[] = [];
        for (const [name, expr] of Object.entries(step.set as Obj)) {
          const v = await this.eval(expr);
          this.values.set(name, v);
          done.push(`${name} = ${v}`);
        }
        return { succeeded: true, outcome: "ok", text: done.join(", ") };
      }
      case "assert": {
        const wrong: string[] = [];
        for (const c of step.assert as Json[]) {
          if (!isObj(c)) continue;
          const got = await this.eval(c.value!);
          const want = await this.interp(String(c.equals));
          if (got !== want) wrong.push(`${got} != ${want}`);
        }
        return { succeeded: wrong.length === 0, outcome: wrong.length ? "failed" : "ok", text: wrong.join("; ") };
      }
      case "override":
        await this.override(step.override as Obj, label);
        return { succeeded: true, outcome: "ok", text: "override applied" };
      case "envelope": {
        const tx = TransactionBuilder.fromXdr(await this.interp(String(step.envelope)), this.ctx.passphrase) as Transaction;
        return this.send(tx, step);
      }
      default:
        return this.transaction(step, label, action);
    }
  }

  private async advance(a: Obj): Promise<Outcome> {
    if (a.ledgers !== undefined) {
      await this.ctx.call("fork_advanceLedgers", { ledgers: Number(a.ledgers), seconds: a.seconds ?? undefined });
    } else if (a.to_ledger !== undefined) {
      await this.ctx.call("fork_advanceLedgers", { toLedger: Number(await this.eval(a.to_ledger)) });
    } else if (a.to_time !== undefined) {
      await this.ctx.call("fork_setTime", { time: Number(await this.eval(a.to_time)) });
    } else if (a.seconds !== undefined) {
      await this.ctx.call("fork_setTime", { time: Number(await this.lookup("time")) + Number(a.seconds) });
    } else {
      throw new PlanError("advance needs ledgers, seconds, to_ledger or to_time");
    }
    return { succeeded: true, outcome: "ok", text: "clock moved" };
  }

  private async override(o: Obj, label: string): Promise<void> {
    const name = typeof o.label === "string" ? o.label : label;
    let n = 0;
    const s = o.substitute_master_key;
    if (isObj(s)) {
      await this.ctx.call("fork_addSigner", { account: await this.eval(s.account!), signer: this.key(String(s.key)).publicKey(), label: name });
      n += 1;
    }
    const ss = o.substitute_signer;
    if (isObj(ss)) {
      await this.ctx.call("fork_addSigner", {
        account: await this.eval(ss.account!),
        signer: this.key(String(ss.key)).publicKey(),
        replace: await this.eval(ss.signer!),
        label: name,
      });
      n += 1;
    }
    const f = o.fund;
    if (isObj(f)) {
      // A plan's fund adds to the balance; the facade sets it.
      const account = await this.eval(f.account!);
      const add = f.xlm !== undefined ? BigInt(await this.eval(f.xlm)) * 10_000_000n : BigInt(await this.eval(f.stroops!));
      const current = await this.balance(account);
      await this.ctx.call("fork_setBalance", { account, stroops: String((current ?? 0n) + add), label: name });
      n += 1;
    }
    const se = o.set_entry;
    if (isObj(se)) {
      await this.ctx.call("fork_setLedgerEntry", {
        entry: await this.interp(String(se.xdr)),
        liveUntilLedgerSeq: se.live_until === undefined ? undefined : Number(await this.eval(se.live_until)),
        label: name,
      });
      n += 1;
    }
    if (typeof o.remove_entry === "string") {
      await this.ctx.call("fork_removeLedgerEntry", { key: await this.interp(o.remove_entry), label: name });
      n += 1;
    }
    if (n !== 1) throw new PlanError(`${name}: an override names exactly one of substitute_master_key, substitute_signer, fund, set_entry, remove_entry`);
  }

  private async balance(account: string): Promise<bigint | undefined> {
    const key = xdr.LedgerKey.account(new xdr.LedgerKeyAccount({ accountId: Keypair.fromPublicKey(account).xdrAccountId() }));
    const entry = (await this.server.getLedgerEntries(key)).entries[0];
    if (!entry || entry.val.type !== "account") return undefined;
    return BigInt(entry.val.account.balance.toString());
  }

  /** The operation a transaction step names. */
  private async operation(step: Obj, action: string): Promise<xdr.Operation | "skip"> {
    if (action === "invoke") {
      const call = step.invoke as Obj;
      return Operation.invokeContractFunction({
        contract: await this.interp(String(call.contract)),
        function: await this.interp(String(call.fn)),
        args: await Promise.all(((call.args as Json[]) ?? []).map((a) => this.scval(a))),
      });
    }
    if (action === "upload") {
      const u = step.upload as Obj;
      const code = await this.wasmFor(u);
      if (u.skip_if_present !== false && (await this.codeExists(sha256(code)))) return "skip";
      return Operation.uploadContractWasm({ wasm: code as unknown as Buffer });
    }
    if (action === "deploy") {
      const d = step.deploy as Obj;
      return Operation.createCustomContract({
        address: new Address(await this.interp(String(d.deployer))),
        wasmHash: unhex(await this.interp(String(d.wasm_hash)), "wasm_hash") as unknown as Buffer,
        salt: unhex(await this.eval(d.salt!), "salt") as unknown as Buffer,
        constructorArgs: await Promise.all(((d.args as Json[]) ?? []).map((a) => this.scval(a))),
      });
    }
    if (step.deploy_sac !== "native") throw new PlanError("deploy_sac supports `native` only");
    return Operation.createStellarAssetContract({ asset: Asset.native() });
  }

  /** An upload's Wasm: the attached file it names, or the one with its hash. */
  private async wasmFor(u: Obj): Promise<Uint8Array> {
    const want = u.hash === undefined ? undefined : (await this.interp(String(u.hash))).toLowerCase();
    if (typeof u.file === "string") {
      const base = (await this.interp(u.file)).split("/").pop()!;
      const code = this.ctx.wasm.get(base);
      if (!code) throw new PlanError(`upload ${u.file}: attach ${base} with the plan`);
      if (want && hex(sha256(code)) !== want) throw new PlanError(`${base} has hash ${hex(sha256(code))}, the plan wants ${want}`);
      return code;
    }
    if (!want) throw new PlanError("upload needs file or hash");
    for (const code of this.ctx.wasm.values()) if (hex(sha256(code)) === want) return code;
    throw new PlanError(`upload: no attached Wasm has hash ${want}`);
  }

  private async codeExists(codeHash: Uint8Array): Promise<boolean> {
    const key = xdr.LedgerKey.contractCode(new xdr.LedgerKeyContractCode({ hash: codeHash as unknown as Buffer }));
    return (await this.server.getLedgerEntries(key)).entries.length > 0;
  }

  private async transaction(step: Obj, label: string, action: string): Promise<Outcome> {
    const op = await this.operation(step, action);
    if (op === "skip") return { succeeded: true, outcome: "skipped", text: "code is already on the ledger" };
    const source = await this.interp(String(step.source ?? this.options.source ?? ""));
    if (!StrKey.isValidEd25519PublicKey(source)) throw new PlanError(`${label}: no G-address source (step.source or options.source)`);
    // build() bumps its account's sequence, so every build starts from the
    // session's current one.
    const sequence = (await this.server.getAccount(source)).sequenceNumber();
    const build = (operation: xdr.Operation) =>
      new TransactionBuilder(new Account(source, sequence), { fee: "100", networkPassphrase: this.ctx.passphrase })
        .addOperation(operation)
        .setTimeout(0)
        .build();
    let tx = build(op);
    let sim = await this.server.simulateTransaction(tx);
    if (rpc.Api.isSimulationError(sim)) return { succeeded: false, outcome: "refused", text: sim.error };
    // Sign every address credential, then simulate again so the signature
    // checks are metered into the declared resources.
    const recorded = sim.result?.auth ?? [];
    if (recorded.some((a) => a.credentials.type !== "sorobanCredentialsSourceAccount")) {
      const info = (await this.ctx.call("fork_info", {})) as { next: { ledger: number } };
      const expiration = info.next.ledger + Number(step.auth_expiration_ledgers ?? this.options.auth_expiration_ledgers ?? 100);
      const signed = await Promise.all(recorded.map((entry) => this.signAuth(entry, step, expiration)));
      if (op.body.type !== "invokeHostFunction") throw new PlanError(`${label}: not an InvokeHostFunction operation`);
      tx = build(Operation.invokeHostFunction({ func: op.body.invokeHostFunctionOp.hostFunction, auth: signed }));
      sim = await this.server.simulateTransaction(tx);
      if (rpc.Api.isSimulationError(sim)) return { succeeded: false, outcome: "refused", text: sim.error };
    }
    const ready = rpc.assembleTransaction(tx, sim).build();
    for (const k of await this.envelopeSigners(step, source)) ready.sign(k);
    if (typeof step.save_envelope === "string") this.values.set(step.save_envelope, ready.toEnvelope().toXdr("base64"));
    if (step.hold) return { succeeded: true, outcome: "held", text: "prepared and signed, not submitted" };
    return this.send(ready, step);
  }

  private async signAuth(entry: xdr.SorobanAuthorizationEntry, step: Obj, expiration: number): Promise<xdr.SorobanAuthorizationEntry> {
    const creds = entry.credentials;
    if (creds.type === "sorobanCredentialsSourceAccount") return entry;
    if (creds.type === "sorobanCredentialsAddressWithDelegates") {
      throw new PlanError("delegated (CAP-71) authorization needs `soroban-sim session run`");
    }
    const inner = creds.type === "sorobanCredentialsAddress" ? creds.address : creds.addressV2;
    const address = Address.fromScAddress(inner.address).toString();
    const keys = await this.classicKeys(address, step);
    if (!keys) throw new PlanError(`${address} must authorize this call, but the plan registers no classic signer for it`);
    return authorizeEntry(
      entry,
      async (_preimage, payload) => ({
        signatureScVal: xdr.ScVal.scvVec(
          [...keys]
            .sort((a, b) => compareBytes(a.rawPublicKey(), b.rawPublicKey()))
            .map((k) =>
              xdr.ScVal.scvMap([
                new xdr.ScMapEntry({ key: xdr.ScVal.scvSymbol("public_key"), val: xdr.ScVal.scvBytes(k.rawPublicKey()) }),
                new xdr.ScMapEntry({ key: xdr.ScVal.scvSymbol("signature"), val: xdr.ScVal.scvBytes(k.sign(payload as unknown as Buffer)) }),
              ]),
            ),
        ),
      }),
      expiration,
      this.ctx.passphrase,
    );
  }

  /** The classic signer keys registered for `address` (step auth first). */
  private async classicKeys(address: string, step: Obj): Promise<Keypair[] | undefined> {
    const specs: Array<[string, Json]> = [...Object.entries(isObj(step.auth) ? step.auth : {}), ...Object.entries(this.signers)];
    for (const [addr, spec] of specs) {
      if ((await this.interp(addr)) !== address) continue;
      checkSigner(addr, spec);
      const names = Array.isArray((spec as Obj).classic) ? ((spec as Obj).classic as string[]) : (((spec as Obj).classic as Obj).keys as string[]);
      return names.map((n) => this.key(n));
    }
    return undefined;
  }

  private async envelopeSigners(step: Obj, source: string): Promise<Keypair[]> {
    if (Array.isArray(step.sign_with)) return step.sign_with.map((n) => this.key(String(n)));
    const classic = await this.classicKeys(source, step);
    if (classic) return classic;
    for (const k of this.keys.values()) if (k.publicKey() === source) return [k];
    throw new PlanError(`no key signs for source ${source}: add sign_with, a classic signer for it, or use a key's own account`);
  }

  private async send(tx: Transaction, step: Obj): Promise<Outcome> {
    const sent = await this.server.sendTransaction(tx);
    if (typeof step.save_envelope === "string" && !this.values.has(step.save_envelope)) {
      this.values.set(step.save_envelope, tx.toEnvelope().toXdr("base64"));
    }
    if (sent.status !== "PENDING") {
      // The session's own step says why, in the CLI's words (txBAD_SEQ, …).
      return { succeeded: false, outcome: "rejected", text: await this.lastStepDetail(sent.status) };
    }
    const got = await this.server.getTransaction(sent.hash);
    if (got.status === rpc.Api.GetTransactionStatus.SUCCESS) {
      const value = got.returnValue ?? xdr.ScVal.scvVoid();
      return { succeeded: true, outcome: "applied", text: render(value), value };
    }
    return { succeeded: false, outcome: "failed", text: await this.lastStepDetail(got.status) };
  }

  private async lastStepDetail(fallback: string): Promise<string> {
    const r = (await this.ctx.call("fork_steps", {})) as { steps: Array<{ outcome: string; detail: string }> };
    const last = r.steps[r.steps.length - 1];
    return last ? `${last.outcome}: ${last.detail}` : fallback;
  }

  private async simulateCall(call: Obj): Promise<Outcome> {
    const viewer = throwawayKeypair("soroban-sim plan viewer").publicKey();
    const tx = new TransactionBuilder(new Account(viewer, "0"), { fee: "100", networkPassphrase: this.ctx.passphrase })
      .addOperation(
        Operation.invokeContractFunction({
          contract: await this.interp(String(call.contract)),
          function: await this.interp(String(call.fn)),
          args: await Promise.all(((call.args as Json[]) ?? []).map((a) => this.scval(a))),
        }),
      )
      .setTimeout(0)
      .build();
    const sim = await this.server.simulateTransaction(tx);
    if (rpc.Api.isSimulationError(sim)) return { succeeded: false, outcome: "failed", text: sim.error };
    const value = sim.result?.retval ?? xdr.ScVal.scvVoid();
    return { succeeded: true, outcome: "ok", text: render(value), value };
  }

  /** Compare an outcome with the step's `expect`; returns why it failed. */
  private async judge(step: Obj, o: Outcome): Promise<string | undefined> {
    const expect = isObj(step.expect) ? step.expect : {};
    let passed = o.succeeded;
    let why = `${o.outcome}: ${o.text}`;
    if (expect.fails !== undefined && expect.fails !== false) {
      passed = !o.succeeded;
      if (typeof expect.fails === "string") {
        const needle = await this.interp(expect.fails);
        if (!o.text.includes(needle)) {
          passed = false;
          why = `${why} (expected it to mention ${JSON.stringify(needle)})`;
        }
      }
      if (o.succeeded) why = `${o.outcome}, but the plan expected it to fail`;
    }
    if (o.succeeded && o.value) {
      if (expect.equals !== undefined) {
        const want = await this.interp(String(expect.equals));
        if (render(o.value) !== want) {
          passed = false;
          why = `${render(o.value)} (expected ${want})`;
        }
      }
      if (expect.value !== undefined) {
        const want = await this.scval(expect.value);
        if (want.toXdr("base64") !== o.value.toXdr("base64")) {
          passed = false;
          why = `${render(o.value)} (expected ${render(want)})`;
        }
      }
    }
    if (passed && typeof step.save === "string" && o.succeeded && o.value) this.values.set(step.save, render(o.value));
    return passed ? undefined : why;
  }
}

function keyFor(name: string, spec: Json): Keypair {
  if (spec === "ed25519") return throwawayKeypair(name);
  if (isObj(spec) && typeof spec.ed25519 === "string" && Object.keys(spec).length === 1) return throwawayKeypair(spec.ed25519);
  throw new PlanError(
    `key ${name}: only ed25519 throwaway keys run in the browser; secp256r1 and WebAuthn keys need \`soroban-sim session run\``,
  );
}

function checkSigner(address: string, spec: Json): void {
  if (isObj(spec) && "classic" in spec) {
    const c = spec.classic;
    if (Array.isArray(c)) return;
    if (isObj(c) && Array.isArray(c.keys) && !c.address_bound) return;
    if (isObj(c) && c.address_bound) {
      throw new PlanError(`signer ${address}: address-bound (CAP-71) classic signatures need \`soroban-sim session run\``);
    }
  }
  throw new PlanError(
    `signer ${address}: only classic signers run in the browser; OpenZeppelin, external and delegated signers need \`soroban-sim session run\``,
  );
}

function compareBytes(a: Uint8Array, b: Uint8Array): number {
  for (let i = 0; i < Math.min(a.length, b.length); i += 1) if (a[i] !== b[i]) return a[i]! - b[i]!;
  return a.length - b.length;
}

/** The contract id `deployer` gets for `salt` on the network `passphrase`. */
function contractId(passphrase: string, deployer: string, salt: Uint8Array): string {
  const preimage = xdr.HashIdPreimage.envelopeTypeContractId(
    new xdr.HashIdPreimageContractId({
      networkId: hash(utf8(passphrase) as unknown as Buffer),
      contractIdPreimage: xdr.ContractIdPreimage.contractIdPreimageFromAddress(
        new xdr.ContractIdPreimageFromAddress({ address: new Address(deployer).toScAddress(), salt: salt as unknown as Buffer }),
      ),
    }),
  );
  return StrKey.encodeContract(hash(preimage.toXdr()));
}
