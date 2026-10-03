export const E2E_CARDS_ID = 'e2e-cards';
/** Formal call/result pairs, using the public ToolPresentation wire shape. */
export function toolCardsFixture():string {
  const tools=[
    {name:'Edit',input:{path:'src/config.rs',edits:[{old_text:'timeout = 10',new_text:'timeout = 30'},{old_text:'legacy = true',new_text:'',all:true}]},content:'Updated src/config.rs: 2 replacements',kind:'Edit'},
    {name:'PlanDraft',input:{content:'# 验证计划\n\n1. 检查配置\n2. **运行测试**'},content:'Plan draft saved',kind:'Control',presentation:{kind:'plan',data:{transition:'draft_saved',content:'# 验证计划\n\n1. 检查配置\n2. **运行测试**'}}},
    {name:'TodoRead',input:{include_completed:true},content:'<todo-snapshot revision="3" pending="1" in_progress="1" completed="1">\n- [pending] T1: 检查配置\n- [in_progress] T2: 运行测试\n- [completed] T3: 记录结果\n</todo-snapshot>',kind:'Control',presentation:{kind:'todo',data:{revision:3,counts:{pending:1,in_progress:1,completed:1},items:[{id:'T1',content:'检查配置',status:'pending'},{id:'T2',content:'运行测试',status:'in_progress'},{id:'T3',content:'记录结果',status:'completed'}],changes:[]}}},
    {name:'PythonSandbox',input:{script:'print("checks passed")'},content:'checks passed',kind:'Command',exitCode:0},
    {name:'SubAgent',input:{prompt:'**检查**公共 API',fork:true},content:'[sub-agent fixture] succeeded\nText: **接口兼容**，验证完成。',kind:'SubAgent'},
    {name:'Bash',input:{command:'cargo test -p fixture',timeout:30},content:'test result: ok. 8 passed; 0 failed\n完整输出：artifact://fixture-output',kind:'Command',exitCode:0},
    {name:'Grep',input:{pattern:'timeout',path:'src',glob:'*.rs'},content:'src/config.rs:10: timeout = 30',kind:'Search'},
    {name:'Write',input:{path:'src/config.rs',content:'timeout = 30\n'},content:'Wrote src/config.rs',kind:'FileWrite'}
  ];
  const rows:Record<string,unknown>[]=[{role:'user',content:'检查工具卡片：编辑、计划、待办、执行和子任务'}];
  tools.forEach((tool,index)=>{
    rows.push({role:'assistant',content:[{type:'tool_use',id:`fixture-${index}`,name:tool.name,input:tool.input}]});
    rows.push({role:'user',content:[{type:'tool_result',tool_use_id:`fixture-${index}`,content:tool.content,_mink:{tool_name:tool.name,status:{state:'succeeded'},result_kind:tool.kind,exit_code:tool.exitCode??null,presentation:tool.presentation??null,artifacts:[]}}]});
  });
  rows.push({role:'assistant',content:[{type:'text',text:'已完成配置检查与验证。'}]});
  return rows.map(row=>JSON.stringify(row)).join('\n')+'\n';
}
