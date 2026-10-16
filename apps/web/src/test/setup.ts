// jsdom implements no ResizeObserver, and the frame only needs to be told the
// column box changed: the stub observes nothing, so unit tests exercise the
// initial solve while the browser lane covers live resizes.

class ResizeObserverStub implements ResizeObserver {
  observe(): void {}
  unobserve(): void {}
  disconnect(): void {}
}

if (!('ResizeObserver' in globalThis)) {
  globalThis.ResizeObserver = ResizeObserverStub
}

// Node's own experimental `localStorage` global can shadow jsdom's without the
// Storage methods (it needs `--localstorage-file`), so preferences would throw
// on read. Install a plain in-memory storage whenever the global is unusable.
class MemoryStorage implements Storage {
  private entries = new Map<string, string>()

  get length(): number {
    return this.entries.size
  }

  clear(): void {
    this.entries.clear()
  }

  getItem(key: string): string | null {
    return this.entries.get(key) ?? null
  }

  key(index: number): string | null {
    return [...this.entries.keys()][index] ?? null
  }

  removeItem(key: string): void {
    this.entries.delete(key)
  }

  setItem(key: string, value: string): void {
    this.entries.set(key, String(value))
  }
}

if (typeof globalThis.localStorage?.setItem !== 'function') {
  Object.defineProperty(globalThis, 'localStorage', {
    value: new MemoryStorage(),
    configurable: true,
  })
}

// jsdom implements neither object-URL side, and the runtime's own compat shim
// rejects jsdom's File objects; the draft rail only needs a URL-shaped string,
// so the stand-in replaces whatever is installed.
let objectUrlCounter = 0
URL.createObjectURL = () => {
  objectUrlCounter += 1
  return `blob:mink-test/${objectUrlCounter}`
}
URL.revokeObjectURL = () => {}

// jsdom implements no scrollIntoView either, and the copied pickers scroll their
// highlighted row into view whenever the highlight moves.
if (typeof Element.prototype.scrollIntoView !== 'function') {
  Element.prototype.scrollIntoView = () => {}
}
