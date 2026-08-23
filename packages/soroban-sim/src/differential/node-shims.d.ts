// Minimal ambient declarations for the Node built-ins used by the differential
// test harness. The package intentionally ships no `@types/node` (tsconfig sets
// `"types": []`), so the handful of Node APIs the harness needs are declared
// here rather than pulling in the full Node typings surface.

declare module "node:fs" {
  export function readFileSync(path: string | URL, encoding: "utf8"): string;
  export function readFileSync(path: string | URL): Uint8Array;
}

declare module "node:fs/promises" {
  export function readFile(path: string | URL): Promise<Uint8Array>;
}

declare module "node:url" {
  export function fileURLToPath(url: string | URL): string;
}
