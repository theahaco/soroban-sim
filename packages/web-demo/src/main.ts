const result = document.querySelector<HTMLPreElement>("#result");
if (!result) {
  throw new Error("missing #result");
}

const workerStarted = performance.now();
let workerStartupMs = 0;
const worker = new Worker(new URL("./worker.ts", import.meta.url), { type: "module" });
worker.onmessage = (event: MessageEvent) => {
  if (event.data?.type === "ready") {
    workerStartupMs = performance.now() - workerStarted;
    worker.postMessage({ type: "run-fixture" });
    return;
  }
  result.dataset.status = "done";
  result.textContent = JSON.stringify(
    { ...event.data, workerStartupMs },
    null,
    2,
  );
  // Hard-cancel path: terminating the Worker is the only way to stop a
  // synchronous component run mid-flight. The SorobanSimulator's AbortSignal
  // only interrupts async provider round-trips; a wasm hang needs terminate().
  worker.terminate();
};
worker.onerror = (event) => {
  result.dataset.status = "error";
  result.textContent = event.message;
  worker.terminate();
};
