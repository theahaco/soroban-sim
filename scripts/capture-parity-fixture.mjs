#!/usr/bin/env node
/**
 * capture-parity-fixture.mjs — capture a REAL Stellar RPC `simulateTransaction`
 * result (plus the ledger state needed to replay it offline) into a committed
 * JSON fixture, for the M3 RPC-equivalence differential harness (#14).
 *
 * The fixture is a self-contained, deterministic snapshot: it carries the RPC's
 * answer (retval, footprint, resources, minResourceFee, auth, events) AND the
 * full ledger context + a state snapshot (footprint entries + every
 * `ConfigSetting` entry `NetworkConfig::load_from_snapshot` reads) so that
 * `soroban-sim`'s `FromLedgerEntries` path can assemble the network config and
 * run without a single `needs-state` round trip.
 *
 * Usage:
 *   node scripts/capture-parity-fixture.mjs [case]     # default: native-sac-decimals
 *   pnpm run capture:parity
 *
 * The script lives at the repo root under scripts/ but the `@stellar/stellar-sdk`
 * dependency is installed only under packages/soroban-sim/node_modules (pnpm does
 * not hoist it to the repo root). ESM bare-specifier resolution walks up from the
 * *script's* directory, not the cwd, so we resolve the SDK explicitly against the
 * package via createRequire and dynamic-import the resolved path. This makes the
 * script cwd-independent.
 *
 * ---------------------------------------------------------------------------
 * v17 SDK gotchas encoded here (verified live against soroban-testnet):
 *   - XDR (de)serialization methods are lower-camel `toXdr` / `fromXdr`
 *     (NOT `toXDR`/`fromXDR`). `retval.toXdr("base64")` yields canonical XDR;
 *     `scValToNative(retval)` yields the native value (7 for decimals()).
 *   - Objects returned by the SDK expose xdr struct/union *fields as own
 *     properties*, not accessor methods (e.g. `td.resources.footprint.readOnly`,
 *     `resources.instructions`), while `switch()`/arm accessors are absent on
 *     those wrappers. We navigate by property and re-parse via our imported
 *     `xdr` where an accessor is genuinely needed.
 *   - `ConfigSettingId` enum members are *values* (objects), used without a call:
 *     `xdr.ConfigSettingId.configSettingStateArchival`.
 * ---------------------------------------------------------------------------
 */
import { createRequire } from "node:module";
import { pathToFileURL, fileURLToPath } from "node:url";
import { writeFileSync, mkdirSync } from "node:fs";
import { createHash } from "node:crypto";
import { join, dirname } from "node:path";

const repoRoot = fileURLToPath(new URL("..", import.meta.url));
const pkgBase = join(repoRoot, "packages", "soroban-sim", "package.json");

// Resolve @stellar/stellar-sdk against packages/soroban-sim, then dynamic-import
// the resolved module path so this works no matter the current directory.
const require = createRequire(pkgBase);
const sdkEntry = require.resolve("@stellar/stellar-sdk");
const sdk = await import(pathToFileURL(sdkEntry).href);
const {
  rpc,
  xdr,
  Networks,
  Keypair,
  TransactionBuilder,
  Contract,
  Address,
  BASE_FEE,
  Asset,
  scValToNative,
  nativeToScVal,
} = sdk;

// --------------------------------------------------------------------------
// Case registry. Each case knows how to name the fixture and build the target
// contract address + host function. Default is the native Stellar Asset
// Contract's read-only `decimals()` (returns u32 7; built-in SAC, no wasm).
// --------------------------------------------------------------------------
const CASES = {
  "native-sac-decimals": {
    file: "native-sac-decimals.json",
    contractId: (passphrase) => Asset.native().contractId(passphrase),
    functionName: "decimals",
    // buildArgs receives runtime context and returns ScVal[] passed to both
    // contract.call(...) and the canonical InvokeContractArgs.
    buildArgs: () => [],
    expectedNative: 7,
  },
  // WRITE+AUTH parity case (#18): native SAC `transfer(from=A, to=B, amount=1)`.
  // A is the tx source account (so A is the required authorizer). Both A and B
  // are funded via friendbot so their classic account entries exist on-ledger;
  // the transfer mutates both native balances → read-write footprint, records
  // A's auth, and emits the SAC "transfer" contract event.
  "native-sac-transfer": {
    file: "native-sac-transfer.json",
    contractId: (passphrase) => Asset.native().contractId(passphrase),
    functionName: "transfer",
    needsDestination: true,
    buildArgs: ({ sourceKp, destKp }) => [
      new Address(sourceKp.publicKey()).toScVal(),
      new Address(destKp.publicKey()).toScVal(),
      nativeToScVal(1n, { type: "i128" }),
    ],
  },
};

const RPC_URL = process.env.SOROBAN_RPC_URL ?? "https://soroban-testnet.stellar.org";
const FRIENDBOT_URL = process.env.FRIENDBOT_URL ?? "https://friendbot.stellar.org";
const NETWORK = "testnet";
const PASSPHRASE = Networks.TESTNET;

// The full set of ConfigSetting IDs that NetworkConfig::load_from_snapshot reads
// (mirrors crates/soroban-sim-core/tests/network_config.rs). Capturing all of
// them lets the FromLedgerEntries path assemble the network config with zero
// needs-state.
const CONFIG_SETTING_IDS = [
  "configSettingContractComputeV0",
  "configSettingContractLedgerCostV0",
  "configSettingContractLedgerCostExtV0",
  "configSettingContractHistoricalDataV0",
  "configSettingContractEventsV0",
  "configSettingContractBandwidthV0",
  "configSettingStateArchival",
  "configSettingContractCostParamsCpuInstructions",
  "configSettingContractCostParamsMemoryBytes",
  "configSettingLiveSorobanStateSizeWindow",
];

/** Standard Stellar base reserve in stroops (0.5 XLM). Confirmed live from the
 * testnet ledger header (`base_reserve: 5000000`). Hard-coded because
 * getLatestLedger does not surface it as a stable top-level field. */
const BASE_RESERVE_STROOPS = 5_000_000;

// --------------------------------------------------------------------------
// Small helpers
// --------------------------------------------------------------------------

/** Retry an async op a few times with linear backoff (transient testnet blips). */
async function withRetry(label, fn, attempts = 5) {
  let lastErr;
  for (let i = 1; i <= attempts; i++) {
    try {
      return await fn();
    } catch (err) {
      lastErr = err;
      const wait = 750 * i;
      console.warn(`  [retry] ${label} failed (attempt ${i}/${attempts}): ${err.message}; waiting ${wait}ms`);
      await new Promise((r) => setTimeout(r, wait));
    }
  }
  throw new Error(`${label} failed after ${attempts} attempts: ${lastErr?.message}`);
}

/** Read a field that may be exposed as a value or a zero-arg accessor. */
function deref(obj, key) {
  const v = obj?.[key];
  return typeof v === "function" ? v.call(obj) : v;
}

/**
 * Wrap an RPC LedgerEntryResult into a full `LedgerEntry` XDR — mirrors
 * RpcStateProvider (packages/soroban-sim/src/providers/rpc.ts): RPC returns the
 * inner LedgerEntryData in `val`; we rebuild the enclosing LedgerEntry so the
 * captured `entryXdr` matches the WIT `ledger-entry.entry-xdr` contract.
 */
function snapshotRecord(result) {
  const entry = new xdr.LedgerEntry({
    lastModifiedLedgerSeq: result.lastModifiedLedgerSeq ?? 0,
    data: result.val,
    ext: xdr.LedgerEntryExt.v0(),
  });
  return {
    keyXdr: result.key.toXdr("base64"),
    entryXdr: entry.toXdr("base64"),
    liveUntilLedger: result.liveUntilLedgerSeq ?? null,
  };
}

// --------------------------------------------------------------------------
// Main
// --------------------------------------------------------------------------
async function main() {
  const caseName = process.argv[2] ?? "native-sac-decimals";
  const spec = CASES[caseName];
  if (!spec) {
    throw new Error(`unknown case '${caseName}'. Known: ${Object.keys(CASES).join(", ")}`);
  }
  console.log(`Capturing parity fixture: ${caseName}`);
  console.log(`  RPC: ${RPC_URL}  network: ${NETWORK}`);

  const server = new rpc.Server(RPC_URL);
  const contractId = spec.contractId(PASSPHRASE);
  console.log(`  contract: ${contractId} fn: ${spec.functionName}()`);

  // 1. Throwaway source account, funded via friendbot.
  const kp = Keypair.random();
  console.log(`  source: ${kp.publicKey()} (funding via friendbot)`);
  const fundViaFriendbot = (addr) =>
    withRetry(`friendbot(${addr.slice(0, 6)}…)`, async () => {
      const res = await fetch(`${FRIENDBOT_URL}/?addr=${encodeURIComponent(addr)}`);
      if (!res.ok) throw new Error(`friendbot HTTP ${res.status}`);
      return res;
    });
  await fundViaFriendbot(kp.publicKey());
  const account = await withRetry("getAccount", () => server.getAccount(kp.publicKey()));

  // Optional destination account (WRITE+AUTH cases like the SAC transfer): fund
  // it too so its classic account entry exists on-ledger and lands in the
  // read-write footprint.
  let destKp = null;
  if (spec.needsDestination) {
    destKp = Keypair.random();
    console.log(`  destination: ${destKp.publicKey()} (funding via friendbot)`);
    await fundViaFriendbot(destKp.publicKey());
    await withRetry("getAccount(dest)", () => server.getAccount(destKp.publicKey()));
  }

  // 2. Build the invoke transaction.
  const contract = new Contract(contractId);
  const args = spec.buildArgs({ sdk, sourceKp: kp, destKp, Address, xdr, nativeToScVal });
  const tx = new TransactionBuilder(account, {
    fee: BASE_FEE,
    networkPassphrase: PASSPHRASE,
  })
    .addOperation(contract.call(spec.functionName, ...args))
    .setTimeout(120)
    .build();
  const txXdr = tx.toEnvelope().toXdr("base64");

  // Canonical host-function XDR (constructed independently so it is stable and
  // exactly what soroban-sim consumes as host-function-xdr).
  const hostFunction = xdr.HostFunction.hostFunctionTypeInvokeContract(
    new xdr.InvokeContractArgs({
      contractAddress: new Address(contractId).toScAddress(),
      functionName: spec.functionName,
      args,
    }),
  );
  const hostFunctionXdr = hostFunction.toXdr("base64");

  // 3. Simulate against the RPC.
  const sim = await withRetry("simulateTransaction", async () => {
    const s = await server.simulateTransaction(tx);
    if (rpc.Api.isSimulationError(s)) {
      throw new Error(`simulation error: ${s.error}`);
    }
    return s;
  });

  // retval — the v17 gotcha: retval has no `toXdr` on some builds, so serialize
  // through our imported xdr for a canonical base64.
  const retval = sim.result.retval;
  const retvalXdr =
    typeof retval.toXdr === "function"
      ? retval.toXdr("base64")
      : xdr.ScVal.fromXdr(retval.toXDR ? retval.toXDR("base64") : retval, "base64").toXdr("base64");
  const retvalNative = scValToNative(retval);
  console.log(`  retval: ${JSON.stringify(retvalNative)} (xdr ${retvalXdr})`);
  if (spec.expectedNative !== undefined && retvalNative !== spec.expectedNative) {
    throw new Error(`retval native ${retvalNative} != expected ${spec.expectedNative}`);
  }

  // Resources + footprint (re-parse transactionData through our xdr so the
  // fields are consistently navigable).
  const td = xdr.SorobanTransactionData.fromXdr(sim.transactionData.build().toXdr("base64"), "base64");
  const resources = td.resources;
  const instructions = Number(deref(resources, "instructions"));
  const readBytes = Number(deref(resources, "diskReadBytes"));
  const writeBytes = Number(deref(resources, "writeBytes"));
  const footprint = resources.footprint;
  const footprintReadOnlyKeys = deref(footprint, "readOnly") ?? [];
  const footprintReadWriteKeys = deref(footprint, "readWrite") ?? [];
  const footprintReadOnly = footprintReadOnlyKeys.map((k) => k.toXdr("base64"));
  const footprintReadWrite = footprintReadWriteKeys.map((k) => k.toXdr("base64"));
  console.log(
    `  footprint: ${footprintReadOnly.length} readOnly / ${footprintReadWrite.length} readWrite; ` +
      `instr=${instructions} readBytes=${readBytes} writeBytes=${writeBytes} minFee=${sim.minResourceFee}`,
  );

  // auth + diagnostic events, canonical base64.
  const auth = (sim.result.auth ?? []).map((a) => a.toXdr("base64"));
  const events = (sim.events ?? []).map((e) => e.toXdr("base64"));

  // 4. Ledger context.
  const latest = await withRetry("getLatestLedger", () => server.getLatestLedger());
  const capturedAtLedger = latest.sequence;
  const protocolVersion = latest.protocolVersion;
  const timestamp = Number(latest.closeTime);
  const networkId = createHash("sha256").update(PASSPHRASE, "utf8").digest("hex");

  // 5. Snapshot: footprint entries + all ConfigSetting entries.
  const configKeys = CONFIG_SETTING_IDS.map((id) =>
    xdr.LedgerKey.configSetting(
      new xdr.LedgerKeyConfigSetting({ configSettingId: xdr.ConfigSettingId[id] }),
    ),
  );
  const footprintKeys = [...footprintReadOnlyKeys, ...footprintReadWriteKeys];
  const allKeys = [...footprintKeys, ...configKeys];

  const ledgerEntries = await withRetry("getLedgerEntries", () => server.getLedgerEntries(...allKeys));
  const snapshot = ledgerEntries.entries.map(snapshotRecord);

  // Count how many snapshot records are ConfigSetting entries (match by the
  // canonical base64 of the config keys we requested; the re-parsed LedgerKey
  // wrappers do not expose a `switch()` accessor).
  const configKeySet = new Set(configKeys.map((k) => k.toXdr("base64")));
  const configCount = snapshot.filter((rec) => configKeySet.has(rec.keyXdr)).length;
  const footprintCount = snapshot.length - configCount;
  console.log(
    `  snapshot: ${snapshot.length} entries (${footprintCount} footprint, ${configCount} config-setting)`,
  );
  if (configCount !== CONFIG_SETTING_IDS.length) {
    console.warn(
      `  [warn] expected ${CONFIG_SETTING_IDS.length} config-setting entries, captured ${configCount}`,
    );
  }

  // TTL bounds from the StateArchival config entry (best-effort; falls back to
  // documented testnet values on any decode drift).
  let minTempEntryTtl = 16;
  let minPersistentEntryTtl = 2073600;
  let maxEntryTtl = 6312000;
  try {
    const saKey = xdr.LedgerKey.configSetting(
      new xdr.LedgerKeyConfigSetting({
        configSettingId: xdr.ConfigSettingId.configSettingStateArchival,
      }),
    );
    const saB64 = saKey.toXdr("base64");
    const saResult = ledgerEntries.entries.find((e) => e.key.toXdr("base64") === saB64);
    const settings = saResult.val.configSetting.stateArchivalSettings;
    minTempEntryTtl = Number(deref(settings, "minTemporaryTtl"));
    minPersistentEntryTtl = Number(deref(settings, "minPersistentTtl"));
    maxEntryTtl = Number(deref(settings, "maxEntryTtl"));
  } catch (err) {
    console.warn(`  [warn] could not extract TTL bounds from StateArchival; using defaults: ${err.message}`);
  }

  const ledgerContext = {
    protocolVersion,
    sequenceNumber: capturedAtLedger,
    // Approximation note: `timestamp` is the close time of the latest ledger at
    // capture, used as the simulation close timestamp (RPC simulate does not pin
    // its own timestamp). `baseReserve` is the standard Stellar 0.5 XLM reserve
    // confirmed from the live ledger header (not surfaced by getLatestLedger).
    timestamp,
    networkId,
    baseReserve: BASE_RESERVE_STROOPS,
    minTempEntryTtl,
    minPersistentEntryTtl,
    maxEntryTtl,
  };

  // 6. Assemble + write.
  const fixture = {
    network: NETWORK,
    networkPassphrase: PASSPHRASE,
    rpcUrl: RPC_URL,
    capturedAtLedger,
    case: caseName,
    contractId,
    functionName: spec.functionName,
    txXdr,
    hostFunctionXdr,
    sourceAccount: kp.publicKey(),
    ...(destKp ? { destinationAccount: destKp.publicKey() } : {}),
    ledgerContext,
    snapshot,
    rpc: {
      retvalXdr,
      minResourceFee: sim.minResourceFee,
      instructions,
      readBytes,
      writeBytes,
      footprintReadOnly,
      footprintReadWrite,
      auth,
      events,
    },
  };

  const outDir = join(repoRoot, "fixtures", "parity");
  mkdirSync(outDir, { recursive: true });
  const outPath = join(outDir, spec.file);
  writeFileSync(outPath, JSON.stringify(fixture, null, 2) + "\n");
  console.log(`\nWrote ${outPath}`);
  console.log(
    `  capturedAtLedger=${capturedAtLedger} protocol=${protocolVersion} ` +
      `footprint=${footprintReadOnly.length}RO/${footprintReadWrite.length}RW ` +
      `snapshot=${snapshot.length}(${configCount} config) minResourceFee=${sim.minResourceFee}`,
  );
}

main().catch((err) => {
  console.error(`\nCAPTURE FAILED: ${err.stack ?? err.message}`);
  process.exit(1);
});
