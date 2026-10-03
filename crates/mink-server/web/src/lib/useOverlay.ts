import { ref, watch, nextTick, onBeforeUnmount, type Ref } from 'vue';

export function useMedia(query: string) {
  const media = window.matchMedia(query);
  const matches = ref(media.matches);
  const update = () => { matches.value = media.matches; };
  media.addEventListener('change', update);
  onBeforeUnmount(() => media.removeEventListener('change', update));
  return matches;
}

function focusableControls(element: HTMLElement | null) {
  return [...element?.querySelectorAll<HTMLElement>('button:not(:disabled),a[href],input:not(:disabled),select:not(:disabled),textarea:not(:disabled),summary,[tabindex="0"]') ?? []].filter(node => {
    const closed = node.closest('details:not([open])');
    return node.tabIndex >= 0 && !node.closest('[hidden],[inert]') && (!closed || closed.querySelector(':scope > summary') === node) && node.getClientRects().length;
  });
}

/** Keep keyboard cycling within a modal, including a native dialog. */
export function cycleFocus(element: HTMLElement | null, event: KeyboardEvent) {
  if (event.key !== 'Tab' || event.isComposing || event.defaultPrevented) return;
  const items = focusableControls(element);
  const first = items[0], last = items.at(-1);
  if (!first) { event.preventDefault(); element?.focus(); }
  else if (event.shiftKey && (document.activeElement === first || !element?.contains(document.activeElement))) { event.preventDefault(); last?.focus(); }
  else if (!event.shiftKey && (document.activeElement === last || !element?.contains(document.activeElement))) { event.preventDefault(); first.focus(); }
}

/** Overlay focus stays local; closing restores the opener without moving the transcript. */
export function useOverlay(element: Ref<HTMLElement | null>, active: Ref<boolean>, close: () => void) {
  let opener: HTMLElement | null = null;
  const controls = () => focusableControls(element.value);
  function key(event: KeyboardEvent) {
    if ((event.target instanceof Element && event.target.closest('[role="menu"]')) || event.defaultPrevented || !active.value || event.isComposing || document.querySelector("dialog[open]")) return;
    if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); close(); }
    cycleFocus(element.value, event);
  }
  // Capture before inert is rendered; move focus only after the DOM update.
  watch(active, value => {
    if (value) opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  }, { immediate: true, flush: 'sync' });
  watch(active, async value => {
    if (value) {
      await nextTick();
      if (active.value) (element.value?.querySelector<HTMLElement>('[data-overlay-focus]') ?? controls()[0] ?? element.value)?.focus({ preventScroll: true });
    } else {
      const previous = opener; opener = null; await nextTick();
      if (!active.value && previous?.isConnected) previous.focus({ preventScroll: true });
    }
  }, { immediate: true, flush: 'post' });
  window.addEventListener('keydown', key);
  onBeforeUnmount(() => { window.removeEventListener('keydown', key); if (active.value) { const previous = opener; void nextTick(() => { if (previous?.isConnected) previous.focus({ preventScroll: true }); }); } });
}
