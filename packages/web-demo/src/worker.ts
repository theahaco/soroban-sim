import { runFixture, SorobanSimulator } from "@theahaco/soroban-sim";
import type { SimulateInput, StateProvider } from "@theahaco/soroban-sim";

async function sha256Hex(bytes: Uint8Array): Promise<string> {
  const data = bytes.slice().buffer;
  const digest = await crypto.subtle.digest("SHA-256", data);
  return [...new Uint8Array(digest)]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
}

/**
 * Build a worker-side StateProvider from serializable config (Topology A).
 * #8 will grow this into RpcStateProvider / InMemoryStateProvider selection;
 * for now an empty config yields an all-absent stub so the loop stays honest.
 */
type ProviderConfig = { kind: "none" } | { kind: "in-memory"; entries?: never };

function buildProvider(_config?: ProviderConfig): StateProvider | undefined {
  // No provider wired yet in the demo; needs-state will surface as an error,
  // which is the correct Topology A behavior until #8 lands.
  return undefined;
}

// Existing M0 golden path: main triggers a single fixture run.
async function handleRunFixture() {
  const started = performance.now();
  const output = runFixture();
  const sha256 = await sha256Hex(output);
  self.postMessage({
    type: "result",
    len: output.length,
    sha256,
    elapsedMs: performance.now() - started,
  });
}

// Topology A: worker owns the rerun loop + provider.
async function handleSimulate(
  id: unknown,
  input: SimulateInput,
  providerConfig?: ProviderConfig,
) {
  const started = performance.now();
  try {
    const provider = buildProvider(providerConfig);
    const sim = await SorobanSimulator.create({
      networkPassphrase: "Test SDF Network ; September 2015",
      provider,
    });
    const result = await sim.simulate(input);
    const sha256 = await sha256Hex(result.resultXdr);
    self.postMessage({
      type: "simulate-result",
      id,
      len: result.resultXdr.length,
      rounds: result.rounds,
      requestedKeyCount: result.requestedKeyCount,
      sha256,
      elapsedMs: performance.now() - started,
    });
  } catch (error) {
    self.postMessage({
      type: "simulate-error",
      id,
      message: error instanceof Error ? error.message : String(error),
    });
  }
}

self.postMessage({ type: "ready" });

self.onmessage = (event: MessageEvent) => {
  const msg = event.data;
  if (msg?.type === "simulate") {
    void handleSimulate(msg.id, msg.input, msg.providerConfig);
    return;
  }
  // Default / { type: "run-fixture" }: preserve the M0 golden path.
  void handleRunFixture();
};
