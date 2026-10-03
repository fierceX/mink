import { beforeEach, describe, expect, it, vi } from 'vitest';
import { mount, flushPromises } from '@vue/test-utils';
import InputBar from './InputBar.vue';
import DetailPanel from './DetailPanel.vue';
import ProcessGroup from './ProcessGroup.vue';
import { appState, attachSession } from '../../lib/store';
import { identity, viewFor, preferences, handoffCommittedView, handoffSnapshotView } from '../../lib/workbench';
import { api } from '../../lib/api';
const summary=(id:string)=>({project_key:'p',id,title:id,alias:null,cwd:'/tmp',corrupt:false,created_at:'',updated_at:'',modified_secs:0,status:'free' as const,path:''});
function attach(id:string) { attachSession(summary(id)); appState.sessionState!.generation='g'; appState.sessionState!.phase='idle'; const view=viewFor(); view.draft=''; view.failures=[]; view.uploads=[]; view.busy=false; view.expanded={}; }
let pending: (value:any)=>void;
beforeEach(()=>{ vi.restoreAllMocks(); vi.stubGlobal('matchMedia',()=>({matches:false})); attach('a'); });
describe('session-owned workbench interactions',()=>{
  it('late failure preserves new drafts and belongs only to its originating session',async()=>{
    vi.spyOn(api,'submitInput').mockImplementation(()=>new Promise(resolve=>{pending=resolve}));
    vi.spyOn(api,'inputs').mockResolvedValue({code:200,data:[],message:''});
    const wrapper=mount(InputBar); const origin=identity();
    await wrapper.find('textarea').setValue('first task'); await wrapper.find('.primary').trigger('click');
    await wrapper.find('textarea').setValue('new draft');
    attach('b'); await wrapper.find('textarea').setValue('B draft');
    pending({code:409,data:null,message:'rejected'}); await flushPromises();
    expect(viewFor(origin).draft).toBe('new draft'); expect(viewFor(origin).failures[0].text).toBe('first task');
    expect(viewFor().draft).toBe('B draft'); expect(viewFor().failures).toHaveLength(0); wrapper.unmount();
  });
  it('IME composition and Shift Enter do not submit; running input remains editable',async()=>{
    const submit=vi.spyOn(api,'submitInput').mockResolvedValue({code:200,data:{} as any,message:''});
    appState.sessionState!.running=true; appState.sessionState!.currentTurn='turn';
    const wrapper=mount(InputBar); await wrapper.find('textarea').setValue('guide');
    await wrapper.find('textarea').trigger('keydown',{key:'Enter',isComposing:true});
    await wrapper.find('textarea').trigger('keydown',{key:'Enter',shiftKey:true});
    expect(submit).not.toHaveBeenCalled(); expect(wrapper.find('textarea').attributes('disabled')).toBeUndefined();
    await wrapper.find('textarea').trigger('keydown',{key:'Enter',ctrlKey:true}); await flushPromises();
    expect(submit).toHaveBeenCalledWith('a',expect.objectContaining({text:'guide',target_turn_id:'turn'}),'p'); wrapper.unmount();
  });
  it('manual process folding survives completion and display-mode changes',async()=>{
    preferences.process='standard'; const wrapper=mount(ProcessGroup,{props:{items:[{kind:'thinking',text:'thought',key:'message:2:0'}],groupKey:'group',active:true}});
    expect(wrapper.attributes('open')).toBeDefined(); await wrapper.find('summary').trigger('click');
    await wrapper.setProps({active:false}); preferences.process='detailed'; await flushPromises();
    expect(wrapper.attributes('open')).toBeUndefined(); preferences.process='standard'; wrapper.unmount();
  });
  it('missing Todo state keeps task details renderable',async()=>{
    vi.spyOn(api,'plan').mockResolvedValue({code:200,message:'',data:{plan:null,draft:null}});
    vi.spyOn(api,'todo').mockResolvedValue({code:200,message:'',data:{todos:null}});
    const wrapper=mount(DetailPanel); await flushPromises(); expect(wrapper.text()).toContain('尚无待办'); expect(wrapper.find('header').text()).toContain('文件'); wrapper.unmount();
  });
  it('formal message handoff retains expansion, anchor and independent inner scroll',()=>{
    const view=viewFor(); view.expanded['process:live:1']=true;view.expanded['thinking:live:1']=true;
    view.anchor={key:'live:1',offset:-20};view.innerScroll[JSON.stringify(['live:1','tp-body md-body'])]=90;
    handoffCommittedView(identity(),[{kind:'thinking',key:'live:1',text:'accepted'}],{type:'conversation_committed',conversation_seq:2,message:{role:'assistant',content:[{type:'thinking',thinking:'accepted'}]}});
    expect(view.expanded['process:message:2:0']).toBe(true);expect(view.expanded['thinking:message:2:0']).toBe(true);expect(view.anchor).toEqual({key:'message:2:0',offset:-20});expect(view.innerScroll[JSON.stringify(['message:2:0','tp-body md-body'])]).toBe(90);
  });

  it('snapshot recovers handoff choices only for newly accepted matching content',()=>{
    const view=viewFor();view.expanded['thinking:live:g:1']=true;
    const items=[{kind:'thinking' as const,key:'live:g:1' as const,text:'accepted thought'}];
    handoffSnapshotView(identity(),items,[{seq:2,role:'assistant',content:[{type:'thinking',thinking:'another candidate'}]}],1);
    expect(view.expanded['thinking:message:2:0']).toBeUndefined();
    handoffSnapshotView(identity(),items,[{seq:3,role:'assistant',content:[{type:'thinking',thinking:'accepted thought'}]}],1);
    expect(view.expanded['thinking:message:3:0']).toBe(true);
  });

});
