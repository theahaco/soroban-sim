import { fileURLToPath } from 'node:url'

export function toPath(file: string | URL): string {
  return file instanceof URL ? fileURLToPath(file) : file
}
