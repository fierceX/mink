import { afterEach, describe, expect, it, vi } from 'vitest';
import { copyText } from './clipboard';
afterEach(() => { vi.restoreAllMocks(); document.body.replaceChildren(); window.getSelection()?.removeAllRanges(); });
function clipboard(value: unknown) { vi.spyOn(navigator, 'clipboard', 'get').mockReturnValue(value as Clipboard); }
// jsdom has no browser clipboard; install a configurable property for the tests.
Object.defineProperty(navigator, 'clipboard', { configurable:true, get:() => undefined });
Object.defineProperty(document, 'execCommand', { configurable:true, writable:true, value:() => false });
describe('text copying', () => {
  it('writes exact Unicode and multiline text with the native API', async () => {
    const writeText=vi.fn().mockResolvedValue(undefined); clipboard({writeText});
    const legacy=vi.spyOn(document,'execCommand'); await copyText('中文\n`code`');
    expect(writeText).toHaveBeenCalledWith('中文\n`code`'); expect(legacy).not.toHaveBeenCalled();
  });
  it('copies synchronously when the API is absent and restores the draft caret', async () => {
    clipboard(undefined); const draft=document.createElement('textarea');draft.value='draft';document.body.append(draft);draft.focus();draft.setSelectionRange(1,3,'backward');
    const legacy=vi.spyOn(document,'execCommand').mockImplementation(command => {
      expect(command).toBe('copy');expect((document.activeElement as HTMLTextAreaElement).value).toBe('中文\nsecond line');return true;
    });
    const pending=copyText('中文\nsecond line');expect(legacy).toHaveBeenCalledOnce();await pending;
    expect(document.activeElement).toBe(draft);expect(draft.selectionStart).toBe(1);expect(draft.selectionEnd).toBe(3);expect(draft.selectionDirection).toBe('backward');expect(document.querySelectorAll('textarea')).toHaveLength(1);
  });
  it('recovers from a native permission rejection and preserves document selection', async () => {
    clipboard({writeText:vi.fn().mockRejectedValue(new DOMException('denied','NotAllowedError'))});
    const span=document.createElement('span');span.textContent='selected text';document.body.append(span);const range=document.createRange();range.selectNodeContents(span);window.getSelection()!.addRange(range);
    vi.spyOn(document,'execCommand').mockReturnValue(true);await copyText('copy me');
    expect(window.getSelection()!.toString()).toBe('selected text');expect(document.querySelector('textarea')).toBeNull();
  });
  it('rejects without claiming success if both methods fail, and cleans up', async () => {
    clipboard({writeText:vi.fn().mockRejectedValue(new Error('denied'))});vi.spyOn(document,'execCommand').mockReturnValue(false);
    await expect(copyText('text')).rejects.toThrow('浏览器未允许复制');expect(document.querySelector('textarea')).toBeNull();
  });
});
