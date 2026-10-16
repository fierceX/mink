/**
 * Request identities, in a form every browser can produce. `crypto.randomUUID`
 * only exists in secure contexts, so a phone opening the workbench over plain
 * HTTP (a LAN address, which is the normal way to use it) has no such function —
 * and every submission would fail before it left the page. `crypto.getRandomValues`
 * is available there, and the fallback keeps the identifier's shape the same.
 */

/**
 * One request id for an input submission.
 * @returns a v4-shaped identifier, unique enough for one session's inputs.
 */
export function requestId(): string {
  const uuid = globalThis.crypto?.randomUUID
  if (typeof uuid === 'function') return uuid.call(globalThis.crypto)
  const bytes = new Uint8Array(16)
  const fill = globalThis.crypto?.getRandomValues
  if (typeof fill === 'function') {
    fill.call(globalThis.crypto, bytes)
  } else {
    for (let index = 0; index < bytes.length; index += 1) bytes[index] = Math.floor(Math.random() * 256)
  }
  // Version 4 and the RFC 4122 variant, so the string reads like a UUID anywhere.
  bytes[6] = (bytes[6] & 0x0f) | 0x40
  bytes[8] = (bytes[8] & 0x3f) | 0x80
  const hex = [...bytes].map(byte => byte.toString(16).padStart(2, '0')).join('')
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`
}
