export const E2E_MOBILE_ID='e2e-mobile';
export const LONG_LINE='const message = "'+'unbroken_token_'.repeat(40)+'";';
export const LONG_FILE=Array.from({length:12},(_,i)=>`${i}: ${LONG_LINE}`).join('\n');
export function mobileFixture():string {
  const text='# 宽度与换行\n\n```ts\n'+LONG_LINE+'\n```\n\n| 文件 | 内容 |\n| --- | --- |\n| '+ 'deep_path_'.repeat(40)+' | '+ 'long_value_'.repeat(40)+' |\n\n'+Array.from({length:18},(_,i)=>`第 ${i+1} 段：手机阅读测试，保持固定阅读宽度和输入草稿。`).join('\n\n');
  return [
    {role:'user',content:'检查手机宽度：'+'long_path_'.repeat(40)},
    {role:'assistant',content:[{type:'tool_use',id:'long-read',name:'Read',input:{path:'long-lines.txt'}}]},
    {role:'user',content:[{type:'tool_result',tool_use_id:'long-read',content:LONG_FILE,_mink:{tool_name:'Read',status:{state:'succeeded'},result_kind:'FileRead',exit_code:null,presentation:null,artifacts:[]}}]},
    {role:'assistant',content:[{type:'text',text}]}
  ].map(row=>JSON.stringify(row)).join('\n')+'\n';
}
