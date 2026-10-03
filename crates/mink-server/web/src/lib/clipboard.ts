/** User-triggered text copy, including HTTP origins without the Clipboard API. */
export async function copyText(text: string): Promise<void> {
  if (navigator.clipboard?.writeText) {
    try { await navigator.clipboard.writeText(text); return; } catch { /* try the selection-based compatibility path */ }
  }
  const active = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  const input = active instanceof HTMLTextAreaElement || active instanceof HTMLInputElement ? active : null;
  const caret = input?.selectionStart != null ? [input.selectionStart, input.selectionEnd!, input.selectionDirection!] as const : null;
  const selection = window.getSelection();
  const ranges = selection ? Array.from({ length: selection.rangeCount }, (_, i) => selection.getRangeAt(i).cloneRange()) : [];
  const target = document.createElement('textarea');
  target.value = text; target.readOnly = true; target.tabIndex = -1;
  target.setAttribute('aria-hidden', 'true');
  Object.assign(target.style, { position:'fixed', top:'0', left:'0', opacity:'0', pointerEvents:'none', fontSize:'16px' });
  document.body.append(target);
  try {
    target.focus({ preventScroll:true }); target.select(); target.setSelectionRange(0, text.length);
    if (!document.execCommand?.('copy')) throw new Error('浏览器未允许复制，请选择文字后手动复制');
  } finally {
    target.remove();
    active?.focus({ preventScroll:true });
    if (input && caret) input.setSelectionRange(...caret);
    selection?.removeAllRanges();
    for (const range of ranges) selection?.addRange(range);
  }
}
