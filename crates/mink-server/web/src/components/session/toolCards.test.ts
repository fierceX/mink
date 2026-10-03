import { beforeEach, describe, expect, it } from 'vitest';
import { mount } from '@vue/test-utils';
import ToolCard from './ToolCard.vue';
import TextResult from './results/TextResult.vue';
import ToolInput from './results/ToolInput.vue';
import { toolSummary, resultViewKind } from '../../lib/toolFormat';
import { appState, attachSession } from '../../lib/store';
import { viewFor } from '../../lib/workbench';
import type { ToolItem } from '../../lib/types';

beforeEach(() => {
  appState.currentSessionId = null; appState.sessionState = null;
  attachSession({ project_key:'cards',id:'cards',title:'cards',alias:null,cwd:'/tmp',created_at:'',updated_at:'',modified_secs:0,status:'free',corrupt:false,path:'' });
  viewFor().expanded = {};
});
function card(name:string, input:unknown, result?:string, presentation?:unknown) {
  const item:ToolItem={kind:'tool',id:'call',key:'message:1:0',name,color:'tool',view:resultViewKind(name),input:JSON.stringify(input),summary:toolSummary(name,input),result,presentation};
  return mount(ToolCard,{props:{item}});
}

describe('structured tool cards',()=>{
  it('renders pending arguments without JSON as the primary view, with reversible raw inspection',async()=>{
    const w=card('Bash',{command:'printf "hello\\n"',timeout:10});
    expect(w.find('.input-content pre').text()).toBe('printf "hello\\n"');
    expect(w.find('.parameter-fields').text()).toContain('超时');
    expect(w.find('.raw-input').attributes('open')).toBeUndefined();
    await w.find('.t-head').trigger('click');expect(w.attributes('open')).toBeDefined();
    await w.find('.t-head').trigger('click');expect(w.attributes('open')).toBeUndefined();
  });
  it('keeps Write content and path after a result arrives',()=>{
    const w=card('Write',{path:'src/config.rs',content:'fn main() {}'},'Wrote 12 bytes');
    expect(w.find('.input-content pre').text()).toBe('fn main() {}');
    expect(w.find('.parameter-fields').text()).toContain('src/config.rs');
    expect(w.text()).toContain('Wrote 12 bytes');expect(w.find('button').text()).toBe('查看当前文件');
    expect(w.find('.t-result').classes()).not.toContain('ok');expect(w.find('.t-status').text()).toBe('状态未记录');
  });
  it.each(['Python','PythonSandbox'])('%s uses real script fields and command output',name=>{
    const w=card(name,{script:'print(1)\nprint(2)'},'1\n2');
    expect(w.find('.input-content pre').text()).toBe('print(1)\nprint(2)');
    expect(w.find('.command-output').text()).toBe('1\n2');
    expect(toolSummary(name,{script:'print(1)\nprint(2)'})).toBe('print(1)');
    expect(toolSummary(name,{script_file:'verify.py'})).toBe('verify.py');
  });
  it('renders every Replace operation, including deletion and all matches',()=>{
    const w=card('Edit',{path:'config.rs',edits:[{old_text:'old()',new_text:'new()'},{old_text:'remove()',new_text:'',all:true}]},'2 replacements');
    expect(w.findAll('.e-replacement')).toHaveLength(2);
    expect(w.findAll('.e-before').map(n=>n.text())).toEqual(['old()','remove()']);
    expect(w.findAll('.e-after').map(n=>n.text())).toEqual(['new()','（空文本）']);
    expect(w.text()).toContain('全部匹配');expect(w.find('.e-original').attributes('open')).toBeUndefined();
    expect(w.find('.t-result').text()).toContain('2 replacements');
  });
  it('retains Hashline and undecodable legacy instructions',()=>{
    const w=card('Edit',{input:'[config.rs#abc]\nPUT 1\nnew()'});
    expect(w.find('.e-lines').exists()).toBe(true);expect(w.find('.e-lines').text()).toContain('new()');
    const legacy=mount(ToolInput,{props:{name:'Custom',input:'legacy opaque input'}});
    expect(legacy.find('.legacy-input').text()).toBe('legacy opaque input');
    expect(toolSummary('Custom',undefined)).toBe('{}');
  });
  it.each(['draft_saved','confirmed'])('renders actual Plan content from %s presentation safely',transition=>{
    const w=card('PlanDraft',{content:'# My plan'},'Plan saved',{kind:'plan',data:{transition,content:'# My plan\n\n**Verify**\n<img src=x onerror=alert(1)>'}});
    expect(w.find('.input-content').exists()).toBe(true); // Different recorded plan bodies remain separately inspectable.
    expect(w.find('.plan-body h1').text()).toBe('My plan');expect(w.find('.plan-body strong').text()).toBe('Verify');
    expect(w.find('.plan-body').html()).not.toContain('onerror');expect(w.find('.plan-original').attributes('open')).toBeUndefined();
  });
  it('shows identical accepted Plan content once, while keeping the call inspectable',()=>{
    const w=card('PlanDraft',{content:'# Same plan'},'Plan saved',{kind:'plan',data:{transition:'draft_saved',content:'# Same plan'}});
    expect(w.find('.input-content').exists()).toBe(false);expect(w.findAll('h1')).toHaveLength(1);
    expect(w.find('.raw-input pre').text()).toContain('# Same plan');
  });
  it('renders Plan clear and legacy Markdown without inventing content',()=>{
    const cleared=card('PlanClear',{},'Plan cleared',{kind:'plan',data:{transition:'cleared',content:null}});
    expect(cleared.find('.plan-transition').text()).toBe('计划已清除');expect(cleared.find('.plan-body').exists()).toBe(false);
    expect(card('PlanDraft',{},'# Legacy plan').find('.plan-body h1').text()).toBe('Legacy plan');
  });
  it('renders Todo presentation revision/counts/items/changes and escapes task content',()=>{
    const task={id:'T1',content:'<img src=x onerror=alert(1)>',status:'in_progress'};
    const w=card('TodoWrite',{base_revision:2,add:['Verify']},'raw todo result',{kind:'todo',data:{revision:3,counts:{pending:2,in_progress:1,completed:4},items:[task],changes:[{change:'added',item:task},{change:'removed',id:'T2'}]}});
    expect(w.find('.t-head-block').text()).toContain('revision 3');expect(w.find('.t-head-block').text()).toContain('已完成 4');
    expect(w.find('.t-tasks').text()).toContain('进行中');expect(w.find('.t-tasks').text()).toContain(task.content);
    expect(w.findAll('.todo-change').map(n=>n.text())).toEqual([`新增 T1 · ${task.content}`,'移除 T2']);expect(w.find('img').exists()).toBe(false);
    expect(w.find('.todo-original').attributes('open')).toBeUndefined();
  });
  it('preserves empty Todo, old XML snapshots and Markdown task lists',()=>{
    const empty=card('TodoRead',{},'empty',{kind:'todo',data:{revision:0,counts:{pending:0,in_progress:0,completed:0},items:[],changes:[]}});
    expect(empty.text()).toContain('没有待办项');
    const xml=card('TodoRead',{},'<todo-snapshot revision="1" pending="1" in_progress="0" completed="0">\n- [pending] T1: Legacy task\n</todo-snapshot>');
    expect(xml.find('.t-task').text()).toContain('Legacy task');
    expect(card('TodoRead',{},'- [ ] Markdown task').find('.md-body li').text()).toContain('Markdown task');
    expect(card('TodoRead',{},'fallback',{kind:'todo',data:{revision:1,items:[null],changes:[null]}}).text()).toContain('fallback');
  });
  it('does not lose generic/SubAgent results, transcript text or artifact access',async()=>{
    const w=card('SubAgent',{prompt:'**Inspect** the API',fork:true},'## Findings\nSafe API\nartifact://a1');
    expect(w.find('.t-result h2').text()).toBe('Findings');expect(w.find('.input-content strong').text()).toBe('Inspect');
    expect(mount(TextResult,{props:{item:{text:'**Reply**'}}}).find('strong').text()).toBe('Reply');
    await w.setProps({item:{...w.props('item'),artifact:'a1'}}); // artifact entry must remain independently available
    expect(w.find('.t-artifact').text()).toBe('artifact://a1');
    expect(card('Custom',{},'**Custom output**').find('.t-result strong').text()).toBe('Custom output');
  });
  it('bounds nested and long parameter rendering with the full raw input still available',()=>{
    const w=card('Custom',{entries:Array.from({length:105},(_,i)=>`entry ${i}`),nested:{a:{b:{c:{d:{e:'deep'}}}}}});
    expect(w.find('.parameter-list').findAll(':scope > li')).toHaveLength(101);
    expect(w.text()).toContain('其余 5 项');expect(w.text()).toContain('嵌套内容请查看原始参数');expect(w.find('.raw-input pre').text()).toContain('entry 104');
  });
});
