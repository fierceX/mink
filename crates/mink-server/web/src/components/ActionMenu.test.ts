import { afterEach, describe, expect, it, vi } from 'vitest';
import { h } from 'vue';
import { mount, flushPromises } from '@vue/test-utils';
import { DropdownMenuItem } from 'reka-ui';
import ActionMenu from './ActionMenu.vue';

const wrappers: ReturnType<typeof mount>[] = [];
afterEach(() => { for (const wrapper of wrappers.splice(0)) wrapper.unmount(); });
function mountMenu() {
  const wrapper = mount(ActionMenu, { attachTo: document.body, props: { label: 'Actions' }, slots: {
    default: () => h(DropdownMenuItem, { asChild: true }, () => h('button', 'Release')),
  } });
  wrappers.push(wrapper); return wrapper;
}
describe('action popup', () => {
  it('supports keyboard entry and Escape restores focus', async () => {
    const wrapper = mountMenu(); const trigger = wrapper.get('button');
    await trigger.trigger('keydown', { key: 'ArrowDown' }); await flushPromises();
    expect(trigger.attributes('aria-expanded')).toBe('true');
    expect(document.activeElement?.textContent).toBe('Release');
    document.activeElement?.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }));
    await flushPromises();
    await vi.waitFor(() => expect(document.activeElement).toBe(trigger.element));
    expect(trigger.attributes('aria-expanded')).toBe('false'); wrapper.unmount();
  });
  it('closes after selecting an action and removes its teleported panel on unmount', async () => {
    const wrapper = mountMenu(); await wrapper.get('button').trigger('click'); await flushPromises();
    (document.querySelector('[role="menuitem"]') as HTMLButtonElement).click(); await flushPromises();
    expect(wrapper.get('button').attributes('aria-expanded')).toBe('false');
    await wrapper.get('button').trigger('click'); await flushPromises(); wrapper.unmount();
    expect(document.querySelector('.action-menu')).toBeNull();
  });
});
