/** Decode display parameters without inventing inputs for legacy summary-only records. */
export function decodeToolInput(input: unknown): Record<string, unknown> | null {
  try {
    const value = typeof input === 'string' ? JSON.parse(input) : input;
    return value && typeof value === 'object' && !Array.isArray(value) ? value as Record<string,unknown> : null;
  } catch { return null; }
}
export const parameterLabels: Record<string,string> = {
  path:'路径',pattern:'匹配规则',glob:'文件规则',type:'文件类型',base_revision:'基于 revision',
  include_completed:'包含已完成项',timeout:'超时（秒）',fork:'继承父会话上下文',description:'任务说明',
  add:'新增',update:'修改',remove:'移除',activate:'激活',complete:'完成',pause:'暂停',reopen:'重新打开',
  id:'ID',content:'内容',status:'状态',script_file:'脚本路径',old_text:'旧文本',new_text:'新文本',all:'替换全部匹配',
};
