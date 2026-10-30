/** Framework-neutral snapshot contract. */

/** Minimal observable snapshot source shared by controllers, stores, and render adapters. */
export interface ObservableSnapshot<T> {
  /** Read the cached snapshot reference. */
  getSnapshot(): T
  /**
   * Subscribe to snapshot invalidation.
   * @param fn - invalidation callback.
   * @returns unsubscribe function.
   */
  subscribe(fn: () => void): () => void
}
