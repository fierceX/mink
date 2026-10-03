import { describe, expect, it } from 'vitest';
import { cachePercentage, diagnosticGroups } from './diagnostics';
import { emptySession } from './types';
import { reduceEvent } from './reducer';
import { reduceWorkbench } from './workbenchReducer';

describe('TUI diagnostic parity', () => {
  it('cache creation is a miss; truncates percentages and preserves unknown / zero states', () => {
    expect(cachePercentage(100,80,10)).toBe(42);
    expect(cachePercentage(0,0,100)).toBe(0);
    expect(cachePercentage(0,0,0)).toBeNull();
    expect(cachePercentage(100,80,undefined)).toBeNull();
  });
  it('restores all status counters and activity from an authoritative running snapshot', () => {
    const state=reduceWorkbench(emptySession('s','s'),{type:'session_snapshot',generation:'new',stream_sequence:4,conversation:[],progress:[],inputs:[],running:true,phase:'running',activity:{work_state:'tool',wait_elapsed_secs:null},diagnostics:{type:'title_update',model:'fixture',stats:{current_turn_count:3,agent_request_count:9,total_input_tokens:100,total_output_tokens:20,total_cache_read_tokens:80,total_cache_creation_tokens:10,current_context_tokens:12,max_context_tokens:100,belief:0.75}},resources:{plan:{plan:'confirmed',draft:null},todo:{items:[{id:'a',status:'in_progress'},{id:'b',status:'pending'}]}}});
    const rows=Object.fromEntries(diagnosticGroups(state,'/fixture').flatMap(group=>group.rows));
    expect(rows).toMatchObject({'模型':'fixture','工作状态':'执行工具','轮次':'3','模型请求':'9','输入':'180','输出':'20','缓存命中率':'42%','缓存创建':'10','上下文占比':'12%','信念度':'0.75','计划':'已确认','Todo':'进行中 1 / 待办 1'});
    const committed=reduceWorkbench(state,{type:'conversation_committed',conversation_seq:1,message:{role:'assistant',content:[{type:'text',text:'accepted'}]},generation:'new',stream_sequence:5});
    expect(committed.workState).toBe('tool');
  });
  it('keeps heartbeat timing through formal commits, clears it on progress, and shows unbounded context', () => {
    let state=emptySession('s','s');state.generation='g';
    state=reduceEvent(state,{type:'info',message:'Waiting for model response... elapsed=23s idle=5s'});
    expect(state.waitElapsedSecs).toBe(23);
    state=reduceWorkbench(state,{type:'conversation_committed',conversation_seq:1,message:{role:'assistant',content:[{type:'text',text:'accepted'}]},generation:'g',stream_sequence:1});
    expect(state.waitElapsedSecs).toBe(23);
    state=reduceEvent(state,{type:'text',content:'progress'});expect(state.waitElapsedSecs).toBeNull();
    state=reduceEvent(state,{type:'title_update',stats:{max_context_tokens:0}});
    expect(Object.fromEntries(diagnosticGroups(state,'').flatMap(group=>group.rows))['上下文上限']).toBe('未限制');
  });
  it('keeps remaining child tasks running and interrupted turns idle like TUI', () => {
    let state=emptySession('s','s');
    state=reduceEvent(state,{type:'sub_agent_status',session_id:'first',status:'launched'});
    state=reduceEvent(state,{type:'sub_agent_status',session_id:'second',status:'running'});
    state=reduceEvent(state,{type:'sub_agent_output',session_id:'first',status:'ok'});
    expect(state.workState).toBe('sub-agent');expect(state.activeSubAgents).toEqual(['second']);
    state=reduceWorkbench(emptySession('s','s'),{type:'session_snapshot',generation:'g',stream_sequence:1,conversation:[],progress:[],inputs:[],running:true,activity:{work_state:'sub-agent',wait_elapsed_secs:null,active_sub_agents:['second','third']}});
    state=reduceWorkbench(state,{type:'sub_agent_output',session_id:'second',status:'ok',generation:'g',stream_sequence:2});
    expect(state.workState).toBe('sub-agent');expect(state.activeSubAgents).toEqual(['third']);
    state=reduceEvent(state,{type:'turn_final',outcome:{status:'interrupted',error:'interrupted'}});
    expect(state.workState).toBe('idle');expect(state.activeSubAgents).toEqual([]);
  });
  it('does not infer missing legacy counters or cache creation', () => {
    const legacy=reduceEvent(emptySession('s','s'),{type:'usage',input_tokens:100,cache_read_input_tokens:80});
    const rows=Object.fromEntries(diagnosticGroups(legacy,'').flatMap(group=>group.rows));
    expect(rows).toMatchObject({'轮次':'—','模型请求':'—','缓存命中率':'—','缓存创建':'—','上下文上限':'—'});
  });
});
