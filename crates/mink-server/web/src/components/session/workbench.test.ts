import { beforeEach, describe, expect, it, vi } from 'vitest';
import { mount, flushPromises } from '@vue/test-utils';
import InputBar from './InputBar.vue';
import DetailPanel from './DetailPanel.vue';
import ProcessGroup from './ProcessGroup.vue';
import { appState, attachSession } from '../../lib/store';
import { identity, viewFor, preferences, handoffCommittedView, handoffSnapshotView } from '../../lib/workbench';
import { api } from '../../lib/api';
const summary=(id:string)=>({project_key:'p',id,title:id,alias:null,cwd:'/tmp',corrupt:false,created_at:'',updated_at:'',modified_secs:0,status:'free' as const,path:''});
function attach(id:string) { attachSession(summary(id)); appState.sessionState!.generation='g'; appState.sessionState!.phase='idle'; const view=viewFor(); view.draft=''; view.failures=[]; view.uploads=[]; view.busy=false; view.expanded={}; view.detailTab='task'; view.detailPath=''; view.detailScroll=0; }
let pending: (value:any)=>void;
beforeEach(()=>{ vi.restoreAllMocks(); vi.stubGlobal('matchMedia',()=>({matches:false,addEventListener:vi.fn(),removeEventListener:vi.fn()})); attach('a'); });
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

  it('stop locks submission immediately and a late acknowledgement does not overwrite the final phase',async()=>{
    vi.spyOn(api,'interrupt').mockImplementation(()=>new Promise(resolve=>{pending=resolve}));
    appState.sessionState!.running=true; appState.sessionState!.currentTurn='turn';
    const wrapper=mount(InputBar); await wrapper.find('textarea').setValue('draft');
    await wrapper.find('.danger').trigger('click');
    expect(wrapper.find('.danger').attributes('disabled')).toBeDefined();
    expect(wrapper.find('.primary').attributes('disabled')).toBeDefined();
    appState.sessionState!.running=false; appState.sessionState!.phase='idle';
    pending({code:200,message:'',data:null}); await flushPromises();
    expect(appState.sessionState!.phase).toBe('idle'); expect(viewFor().draft).toBe('draft'); wrapper.unmount();
  });
  it('receipt actions lock repeated clicks and a late edit cannot replace applied history',async()=>{
    const receipt={input_id:'guide',revision:1,status:'pending',input:{text:'guide'}} as any;
    appState.sessionState!.inputs=[receipt];
    const edit=vi.spyOn(api,'editInput').mockImplementation(()=>new Promise(resolve=>{pending=resolve}));
    const wrapper=mount(InputBar);
    await wrapper.find('.pending-input button').trigger('click');
    await wrapper.find('[aria-label="编辑待处理输入"]').setValue('updated');
    const save=wrapper.findAll('.pending-input button').find(button=>button.text()==='保存')!;
    await save.trigger('click'); await save.trigger('click'); expect(edit).toHaveBeenCalledTimes(1);
    appState.sessionState!.inputs=[{...receipt,status:'applied',revision:3}];
    pending({code:200,message:'',data:{...receipt,revision:2,input:{text:'updated'}}}); await flushPromises();
    expect(appState.sessionState!.inputs[0].status).toBe('applied'); expect(wrapper.find('.pending-input').exists()).toBe(false); wrapper.unmount();
  });
  it('programmatic draft recovery resizes the composer and clearing it restores its height',async()=>{
    const wrapper=mount(InputBar); const textarea=wrapper.find('.input-bar textarea').element as HTMLTextAreaElement;
    Object.defineProperty(textarea,'scrollHeight',{get:()=>viewFor().draft.includes('\n')?130:48});
    viewFor().draft='recovered\nmultiline\ndraft'; await flushPromises(); expect(textarea.style.height).toBe('130px');
    viewFor().draft=''; await flushPromises(); expect(textarea.style.height).toBe('48px'); wrapper.unmount();
  });
  it('details drop stale directory rows, return to the containing directory and expose loading feedback',async()=>{
    viewFor().detailTab='files';
    vi.spyOn(api,'files').mockImplementation(async(_id,path)=>({code:200,message:'',data:path==='src/main.ts'?{content:'hello'}:{items:[{name:path?'main.ts':'src',dir:!path}]}} as any));
    const wrapper=mount(DetailPanel); await flushPromises();
    await wrapper.find('.file-tree button').trigger('click'); await flushPromises(); expect(wrapper.find('.directory-path').text()).toBe('src/');
    await wrapper.find('.file-tree button').trigger('click'); await flushPromises();
    expect(wrapper.find('.file-tree').exists()).toBe(false); expect(wrapper.find('h4').text()).toBe('src/main.ts');
    await wrapper.findAll('.file-ops button').find(button=>button.text()==='返回目录')!.trigger('click'); await flushPromises();
    expect(viewFor().detailPath).toBe('src/'); expect(wrapper.find('.file-tree').text()).toContain('main.ts');
    vi.spyOn(api,'files').mockImplementation(()=>new Promise(resolve=>{pending=resolve}));
    await wrapper.findAll('.file-ops button').find(button=>button.text()==='刷新')!.trigger('click');
    expect(wrapper.find('[role="status"]').text()).toContain('正在加载');
    pending({code:200,message:'',data:{items:[]}}); await flushPromises(); expect(wrapper.text()).toContain('此目录为空'); wrapper.unmount();
  });

  it('a stop response from an earlier generation cannot cancel a newly opened running turn',async()=>{
    vi.spyOn(api,'interrupt').mockImplementation(()=>new Promise(resolve=>{pending=resolve}));
    appState.sessionState!.running=true; appState.sessionState!.currentTurn='old-turn';
    const wrapper=mount(InputBar); await wrapper.find('.danger').trigger('click');
    appState.sessionState!.generation='new-generation'; appState.sessionState!.currentTurn='new-turn'; appState.sessionState!.phase='running';
    pending({code:200,message:'',data:null}); await flushPromises();
    expect(appState.sessionState!.running).toBe(true); expect(appState.sessionState!.phase).toBe('running'); wrapper.unmount();
  });

});

describe('collapsed mobile composer',()=>{
  it('keeps the draft mounted and restores focus without losing content',async()=>{
    viewFor().draft='retained mobile draft';const wrapper=mount(InputBar,{attachTo:document.body,props:{collapsed:true}});
    expect(wrapper.classes()).toContain('collapsed');expect(wrapper.find('textarea').element.value).toBe('retained mobile draft');
    await wrapper.get('[aria-label="显示输入框"]').trigger('click');expect(wrapper.emitted('showComposer')).toHaveLength(1);
    await wrapper.setProps({collapsed:false});await flushPromises();expect(wrapper.find('textarea').element.value).toBe('retained mobile draft');wrapper.unmount();
  });
  it('reveals waiting inputs, failed submissions and uploads even when the parent requests collapse',async()=>{
    const wrapper=mount(InputBar,{props:{collapsed:true}});expect(wrapper.classes()).toContain('collapsed');
    appState.sessionState!.inputs=[{input_id:'guide',revision:1,status:'unapplied',input:{text:'keep visible'}} as any];await flushPromises();expect(wrapper.classes()).not.toContain('collapsed');expect(wrapper.emitted('readingLock')?.at(-1)).toEqual([true]);
    appState.sessionState!.inputs=[];viewFor().uploads=[{key:'upload',name:'image',bytes:1,preview:'',status:'uploading'}];await flushPromises();expect(wrapper.classes()).not.toContain('collapsed');
    viewFor().uploads=[];viewFor().failures=[{id:'failure',text:'task',attachments:[],error:'failed'}];await flushPromises();expect(wrapper.find('[role="alert"]').text()).toContain('failed');expect(wrapper.classes()).not.toContain('collapsed');wrapper.unmount();
  });
  it('retains an actionable Stop in reading mode',async()=>{
    appState.sessionState!.running=true;appState.sessionState!.currentTurn='turn';const interrupt=vi.spyOn(api,'interrupt').mockResolvedValue({code:200,data:null,message:''});
    const wrapper=mount(InputBar,{props:{collapsed:true}});await wrapper.find('.composer-reading-actions [aria-label="停止"]').trigger('click');await flushPromises();expect(interrupt).toHaveBeenCalledWith('a','p');expect(appState.sessionState!.phase).toBe('cancelling');expect(wrapper.classes()).not.toContain('collapsed');wrapper.unmount();
  });
});
