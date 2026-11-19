import { spawn, type ChildProcess } from 'node:child_process'

export interface SimServer {
  url: string
  stop(): Promise<void>
}

const LISTENING = /RPC facade on (http:\/\/\S+?): /

/**
 * Starts `soroban-sim serve ARGS --port 0` on loopback and resolves with its
 * URL once it listens. Each server is its own process, so servers started
 * from one snapshot share nothing.
 */
export function startServer(bin: string, args: string[], timeoutMs = 60_000): Promise<SimServer> {
  const child = spawn(bin, ['serve', ...args, '--host', '127.0.0.1', '--port', '0', '-q'], {
    stdio: ['ignore', 'ignore', 'pipe'],
  })
  let stderr = ''
  const exited = new Promise<void>((resolve) => child.once('exit', () => resolve()))
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      child.kill('SIGKILL')
      reject(new Error(`soroban-sim serve did not start within ${timeoutMs} ms:\n${stderr}`))
    }, timeoutMs)
    const fail = (why: string) => {
      clearTimeout(timer)
      reject(new Error(`${bin} serve ${args.join(' ')} ${why}:\n${stderr.trim()}`))
    }
    child.once('error', (e) => fail(`could not start (${e.message}); set SOROBAN_SIM_BIN to the soroban-sim binary`))
    const early = (code: number | null, signal: NodeJS.Signals | null) => fail(`exited (${signal ?? code}) before listening`)
    child.once('exit', early)
    child.stderr!.setEncoding('utf8')
    child.stderr!.on('data', (chunk: string) => {
      // Keep the tail only: a long-running server's warnings are not needed in full.
      stderr = (stderr + chunk).slice(-16_384)
      const m = LISTENING.exec(stderr)
      if (!m) return
      clearTimeout(timer)
      child.off('exit', early)
      resolve({ url: m[1], stop: () => stop(child, exited) })
    })
  })
}

async function stop(child: ChildProcess, exited: Promise<void>): Promise<void> {
  if (child.exitCode !== null || child.signalCode !== null) return
  child.kill('SIGTERM')
  const timer = setTimeout(() => child.kill('SIGKILL'), 5_000)
  await exited
  clearTimeout(timer)
}

/** Calls a facade method (`fork_*` admin methods included) and returns its result. */
export async function rpcCall<T = unknown>(url: string, method: string, params: Record<string, unknown> = {}): Promise<T> {
  const res = await fetch(url, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }),
  })
  if (!res.ok) throw new Error(`${method}: HTTP ${res.status} ${await res.text()}`)
  const body = (await res.json()) as { result?: T; error?: { code: number; message: string } }
  if (body.error) throw new Error(`${method}: ${body.error.message} (${body.error.code})`)
  return body.result as T
}
