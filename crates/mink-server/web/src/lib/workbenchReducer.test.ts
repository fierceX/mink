import { describe, it, expect } from 'vitest';
import { emptySession } from './types';
import { reduceWorkbench, projectMessages, mergeTodo } from './workbenchReducer';
import { conversationToEvents } from './toolFormat';
import { projectTurns } from './transcriptProjection';
const snapshot = (rows: Record<string,unknown>[] = []) => reduceWorkbench(emptySession('s','s'), {type:'session_snapshot',generation:'g',stream_sequence:0,conversation:rows,progress:[],running:true,current_turn:'t',phase:'running',inputs:[],capabilities:{image_input:{kind:'unsupported'}}});
describe('workbench authority and projection',()=>{
  it('reconnects while running and replaces transient response by formal identity',()=>{
    let state=snapshot([{seq:1,role:'user',content:'task',_mink:{input_id:'task',turn_id:'t'}}]);
    expect(state.desynced).toBe(false); expect(state.running).toBe(true);
    state=reduceWorkbench(state,{type:'text',content:'draft response',stream_sequence:1,generation:'g'});
    state=reduceWorkbench(state,{type:'conversation_committed',conversation_seq:2,message:{role:'assistant',content:[{type:'text',text:'accepted response'}]},generation:'g',stream_sequence:2});
    expect(state.items.filter(item=>item.kind==='text')).toEqual([{kind:'text',key:'message:2:0',text:'accepted response'}]);
    expect(reduceWorkbench(state,{type:'text',content:'old',generation:'old',stream_sequence:99})).toBe(state);
    expect(reduceWorkbench(state,{type:'text',content:'duplicate',generation:'g',stream_sequence:2})).toBe(state);
  });
  it('internal messages do not create turns; guidance stays in its original turn',()=>{
    const rows=[{seq:1,role:'user',content:'task'}, {seq:2,role:'user',internal:true,content:'checkpoint'}, {seq:3,role:'user',content:'guidance',_mink:{input_id:'guide',guidance:true}}, {seq:4,role:'assistant',content:[{type:'text',text:'final'}]}];
    const state=projectMessages(emptySession('s','s'),rows); const turns=projectTurns(state.items);
    expect(turns).toHaveLength(1); expect(turns[0].segments.at(-1)?.kind).toBe('message');
    expect(state.items.find(item=>item.key==='input:guide')).toMatchObject({kind:'user',guidance:true});
  });
  it('history retains structured result metadata and >100 blocks cannot collide',()=>{
    const events=conversationToEvents({seq:5,role:'assistant',content:Array.from({length:150},(_,index)=>({type:'tool_use',id:String(index),name:'Read',input:{path:'src.rs'}}))});
    expect(new Set(events.map(event=>event.key)).size).toBe(150);
    const result=conversationToEvents({seq:6,role:'user',content:[{type:'tool_result',tool_use_id:'1',content:'out',_mink:{status:{state:'failed',kind:'timeout'},presentation:{kind:'plan',data:{content:'plan'}},artifacts:[{id:'a'}]}}]});
    expect(result[0]).toMatchObject({status:{state:'failed'},artifacts:[{id:'a'}],presentation:{kind:'plan'}});
  });
  it('retains chronology of retry and signal evidence across formal commits',()=>{
    let state=snapshot([{seq:1,role:'user',content:'task'}]);
    state=reduceWorkbench(state,{type:'retry',after_conversation_seq:1,generation:'g',stream_sequence:1});
    state=reduceWorkbench(state,{type:'conversation_committed',conversation_seq:2,message:{role:'assistant',content:[{type:'text',text:'accepted'}]},generation:'g',stream_sequence:2});
    expect(state.items.map(item=>item.kind)).toEqual(['user','system','text']);
    const turn=projectTurns(state.items)[0]; expect(turn.segments[0].kind).toBe('message');
  });
  it('a delayed pending receipt cannot duplicate an already committed input',()=>{
    let state=snapshot([{seq:1,role:'user',content:'guide',_mink:{input_id:'g'}}]);
    state=reduceWorkbench(state,{type:'inputs_updated',inputs:[{input_id:'g',status:'pending'}],generation:'g',stream_sequence:1});
    expect(state.inputs![0].status).toBe('applied');
  });
  it('todo deltas preserve completed items and apply explicit removals',()=>{
    const previous={revision:2,items:[{id:'done',content:'completed',status:'completed'},{id:'active',content:'work',status:'in_progress'}]};
    const next=mergeTodo(previous,{revision:3,items:[],changes:[{change:'completed',id:'active'}]});
    expect(next.items).toHaveLength(2); expect(next.items![1].status).toBe('completed');
    expect(mergeTodo(next,{revision:4,items:[],changes:[{change:'removed',id:'done'}]}).items).toHaveLength(1);
    expect(mergeTodo(next,{revision:2,items:[]})).toBe(next);
  });

  it('snapshot replaces transport deduplication so repeated progress survives gap recovery',()=>{
    let state=snapshot([{seq:1,role:'user',content:'task'}]);
    const progress={type:'text',content:'still generating',generation:'g',stream_sequence:1,turn_id:'t',sequence:2};
    state=reduceWorkbench(state,progress);
    state=reduceWorkbench(state,{type:'session_snapshot',generation:'g',stream_sequence:1,conversation:[{seq:1,role:'user',content:'task'}],progress:[progress],inputs:[],current_turn:'t',phase:'running',running:true});
    expect(state.items.at(-1)).toMatchObject({kind:'text',text:'still generating'});
    expect(state.desynced).toBe(false);
  });

  it('different runtime generations never reuse transient reading identities',()=>{
    const left=reduceWorkbench(snapshot(),{type:'thinking',content:'left',generation:'g',stream_sequence:1});
    const right=reduceWorkbench(reduceWorkbench(emptySession('s','s'),{type:'session_snapshot',generation:'new',stream_sequence:0,conversation:[],progress:[],inputs:[],running:true}),{type:'thinking',content:'right',generation:'new',stream_sequence:1});
    expect(left.items[0].key).not.toBe(right.items[0].key);
  });

  it('round usage is taken only from authoritative final outcomes and survives snapshot',()=>{
    const final={type:'turn_final',turn_id:'t',generation:'g',stream_sequence:1,outcome:{turn_id:'t',usage:{request_count:1,reported_request_count:0,unreported_request_count:1,tokens:{input_tokens:0,output_tokens:0}}}};
    const state=reduceWorkbench(snapshot(),final);expect(state.outcomes?.t.usage).toEqual(final.outcome.usage);
    const restored=reduceWorkbench(emptySession('s','s'),{type:'session_snapshot',generation:'g',stream_sequence:1,conversation:[],progress:[],inputs:[],running:false,last_final:final});
    expect(restored.outcomes?.t.usage).toEqual(final.outcome.usage);
  });

});
