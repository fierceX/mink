# Goal 契约

> 更新日期：2026-10-16

Goal 是 Session 内版本化、有边界的跨 turn 目标：目标形成、执行与独立审核分离；GoalStore 是唯一权威状态，`goal.json` 与 `goal-transaction.json` 通过 journal 和原子发布保持一致。普通 turn 不隐式创建或确认目标。

## 契约与草案

`GoalSpec::new(objective)` 创建契约：保留 `original_request`，默认一个 `subgoals` 条目与一个 `criteria` 条目，默认验收 `AgentReviewed`。每个子目标有稳定 `id`、`objective`；每个验收项有 `id`、`subgoal_id`、`outcome`、`required_evidence`。子目标只组织契约，不独立调度或分配预算。`acceptance_criteria` 是 criteria.outcome 的兼容展示，必须一致；简单契约可用 `set_acceptance_criteria()` 更新。`scope`、`constraints` 与 `suggested_actions` 分别记录范围、全局约束与建议动作；建议动作不成为完成条件或 Todo 权威状态。

`/goal` 的子命令为 `confirm` / `refine` / `pause` / `resume` / `cancel` / `clear` / `approve` / `reconcile` / `human` / `verified` / `budget`。参数首词（包括带参数的调用，如 `confrim g12`）与某个子命令只差一次插入、删除、替换或相邻换位时（如 `cancal`、`cancle`、`cance`、`refne`），解析器拒绝并给出建议，不把该词当作目标正文；首词不接近任何子命令的文本（如 `refactor`、`fix the cancal bug`）仍作为正文创建目标。终端与 `apps/web` 两个解析器共用 `crates/mink-cli/src/tui/slash_commands.json` 契约，新增用例两处同时生效。

`draft_goal(spec, duration_secs)` 保存宿主准备好的完整草案；`request_goal_draft(spec, duration_secs)` 从创建时即保存 `draft_ready=false` 的待形成草案，避免生成请求到达前的确认竞争。CLI/Web 默认使用后者，再调用 `generate_goal_draft(feedback)`。它打开普通会话 turn，冻结本轮只读探索权限，允许 Read/Glob/Grep/GoalRead/GoalDraft，禁止执行、工作区写入、普通子代理、自定义工具与自定义资源处理器。模型以 `GoalDraft { draft_id, revision, spec }` 保存 proposal；proposal 不含原始请求、额度或验收方式。每次草案生成开始时 `draft_ready=false`，成功保存后变为 true；只提出澄清问题而未保存有效 proposal 时不能确认。

`confirm_goal(id, revision)` 是宿主 compare-and-set 操作，不向模型暴露。它核对草案、执行权、配置与预算后发布 active；启用驱动时启动唯一 run。默认 20 个执行 turn、60 分钟从确认时开始；草案讨论不消耗执行轮次，模型调用正常进入会话账单。已确认契约不能由执行模型修改，变更范围需取消并重新形成契约。`cancel_goal_draft` 取消待确认草案并中断在途讨论。

状态为 draft/active/paused/blocked/budget_exhausted/completed/cancelled/failed；阶段另存 idle/executing/verifying。completed/cancelled/failed 为终态，一个 Session 最多一个未结束目标。暂停不重置截止时间；调整预算不自动恢复。草案不能通过 pause/resume 绕过确认。

执行失败与 finished/billing/usage 同次原子发布为 failed，表示本 run 已结束，不自动重试，也不进入完成审核。宿主显式 `resume_goal` 可重试同一目标；无需识别错误字符串或区分历史失败版本。原有 iteration、用量、失败/阻断记录、契约身份和已提交工具交换完整保留，不重放已提交调用，不重置已用轮次或原期限。单个请求的自动重试与格式恢复上限保持不变，显式重试是宿主发起的新 run。

恢复仍核对 ID/revision、运行时故障闩锁、执行退出、环境、容量、未决副作用和预算。当前 runtime 的 Faulted 或持久化闩锁拒绝恢复，须关闭并重新打开会话完成恢复检查；未决副作用需先人工对账。failed 允许调整预算，但调整不会激活；过期或轮数耗尽须显式修改后再 resume。completed/cancelled 不可恢复或改预算；draft 必须确认。清除 failed 会隐藏记录，隐藏操作本身不启动执行。

恢复以 phase 决定继续位置：verifying 或保存有审核失败原因时，复核同一候选，最后一个执行轮也可继续审核；其余阶段回到 idle，开启下一执行轮。新版 Web 对 failed 显示“重试目标”，并提供总轮数和从当前时刻起的分钟数调整；未填写项及 Token 软限额保持不变，预算耗尽或副作用未对账时禁用重试。REST/SSE 共用原始 GoalSnapshot，不增加派生权限字段。

| 状态 | resume | 调整预算 | pause | cancel | clear |
|---|---|---|---|---|---|
| draft | 拒绝，需确认 | 允许 | 拒绝 | 允许 | 拒绝 |
| active | 执行权空闲时允许 | 允许 | 允许 | 允许 | 拒绝 |
| paused / blocked / budget_exhausted | 满足恢复门槛时允许 | 允许 | 允许 | 允许 | 拒绝 |
| failed | 满足恢复门槛时显式重试 | 允许 | 拒绝 | 拒绝 | 允许 |
| completed / cancelled | 拒绝 | 拒绝 | 拒绝 | 拒绝 | 允许 |

## 正常结束与独立审核

执行者正常结束一个 Goal turn 后，正式历史 sync 与 `settle_iteration` 原子结算 finished、状态与用量，并进入 verifying。无需 GoalReport，也没有缺报告补全 turn。取消、中断、永久错误、恢复耗尽、信号终止与持久化故障不被审核覆盖为成功。

宿主为本轮创建 `GoalReviewCandidate`：goal_id、contract_revision、iteration、conversation_seq、reviewer_id 与单调递增 generation。conversation_seq 绑定执行轮提交边界；宿主提交轮结束后的正式历史水位另存 acceptance_conversation_seq。每次失效后的重新审核使用新的 generation。审核子代理 fork 已提交的会话投影与摘要、历史和产物，使用当前实际模型与共享 backend；工具面固定为已允许的 Read/Glob/Grep/GoalRead，不包含 GoalReview。它不继承自定义工具、资源处理器或宿主 PrefixSource，不获得写入、执行、Plan/Todo 修改、Goal 控制和递归委托能力。GoalRead 在审核者中读取候选绑定快照。

审核子代理先读取本执行轮最后一条正式 assistant 文本（不拼接过程输出），明确只完成一部分并有契约内下一步时可返回轻量描述性报告而不检查工作区；声称全目标完成、结论空白/含糊/矛盾、下一步属于契约外优化或存在外部阻断时，检查实际证据并覆盖原始要求、每个验收 ID、约束、未完成项、缺失证据及客观外部依赖。运行时将报告保存为父会话 artifact，并导入审核者实际读取的证据和字节版本。随后宿主 agent 在原会话执行专用提交轮，使用固定的主代理工具 schema 和当前活动模型，结合报告调用 `GoalReview` 提交 decision、summary 和未完成时的 next_step。decision 为 continue/complete/blocked；候选身份、成功报告和实际读取的证据版本由 runtime 绑定，不由模型复制。runtime 校验后才更新 GoalStore，审核子代理不能调用此工具。GoalReview 默认出现在主代理的固定工具面中，但普通执行轮无提交权限；显式工具白名单须包含 GoalReview。提交轮的派发范围只允许读取与 GoalReview，不允许执行任务、写文件或自定义资源处理器。


`GoalReview` 使用一个模型输入协议：必填 decision、summary；continue/blocked 需要具体 next_step，blocked 另给 blocker 字符串描述客观外部依赖，其他决策省略 blocker。可选 remaining_requirements 使用 GoalRead 的 requirement_ids；省略时保守地跟踪全部要求。原始要求 ID 为 `request:original`，验收项使用原 ID，约束为 `constraint:1` 等。主代理不用选择 full/continue 模式，也不用重写逐项审核表。独立代理已检查证据但发现缺项时，仍然提交 decision=continue。完成提交必须有独立代理实际读取的证据；报告自述不是充分的机械证明，契约覆盖与证据充分性仍由审核者和宿主判断。旧完整提交仅为已有历史与在途调用保留解码，不再作为当前模型 schema 宣传。

轻量记录独立保存于 continuations，不修改完整 reviews、验收项状态或完成依据。`latest_round_decision()` 返回最近的完整审核或轻量继续。连续三轮剩余要求集合相同则升级证据检查：指纹由 contract revision 与排序后的要求标识生成，不因反馈换词重置；第三次提议只触发升级，不发布第三份轻量记录。同一进程复用原只读子会话与证据，完整审核后清零轻量计数。暂停、重试和重启不清零，同一候选不重复计数；该门槛不判定无进展或阻断。

可选 review_stage 为 screening/inspecting/adjudicating，分别显示分流、证据检查、宿主提交。GoalRead 不算产物检查；审核者请求 Read/Glob/Grep 时进入 inspecting。已有完整审核结果按所属轮次展示，不能表示最新产物已验收。

描述性审核结论在 reviews 中保留绑定的 report，不制造逐项 pass/fail/unknown；旧版逐项审核保留其原有字段及严格校验。界面按所属轮次显示结论和报告入口，子目标细节见报告。新的 complete 需要成功的独立报告与非空独立读取证据，runtime 校验候选、历史边界、报告字节和证据版本，再进入检查器与人工门槛。执行者自述、Todo 完成、单项测试通过均不是全目标完成的充分证据。

已解码的语义拒绝返回具体诊断，宿主同 turn 纠正重试，不计入格式恢复窗口；参数解码或响应协议损坏仍遵循 llm_recovery。宿主自然结束但未提交时，内部提醒继续调用工具，受请求轮数、审核期限与 Goal usage 约束。

continue 将缺口与下一步写入下一执行轮尾部。blocked 需要客观依赖与独立读取证据；runtime 绑定外部类别、契约版本、受影响验收 ID 和依赖事实版本形成持久指纹，默认相同依赖事实连续三轮才停止。依赖文件优先于附带产物；执行结论与审核叙述不能充当依赖事实。描述措辞和新 artifact ID 不改变身份，事实字节或受影响要求变化则重置计数。Glob 不等于 Read 证据。失败持久化 review_failure_reason 和 review_failure_stage：screening/inspecting 是独立代理阶段，adjudicating 是宿主提交阶段。有效报告已保存且版本仍匹配时，resume 只恢复宿主提交；报告或证据变化才重新审核。失败不重新执行已结束任务，也不增加执行轮数。旧 inconclusive 仅保留历史含义。

独立审核与宿主提交均不额外消耗 Goal 执行轮数，但所有请求、摘要和重试进入同一 Goal usage；一次验收编排（含重新审核和检查器）最多 600 秒，并受剩余绝对期限约束。每次请求准入前对账 Token 用量，已知达到软限额后不发起新请求。最后一个允许的执行轮仍可审核，随后不得再接纳超额执行轮。宿主提交报告以带 internal/source/candidate 的 goal_adjudication 消息追加到正式历史，不伪装人类输入，不消费新的人类引导；新引导使完成提交失效。完成与恢复核对执行候选及宿主提交后的水位；压缩只修改活跃投影，正式历史只追加。

## 验收门槛与证据

默认审核满足契约后提交 completed，completion basis 为 `agent_reviewed`。`Verifier { id }` 或 `checker_id` 指定宿主检查器，必须稳定身份匹配；单个注册检查器默认关联新草案，多个注册检查器须显式选择。检查器 pass 后且证据未变才提交 `verified`；缺失、不确定或超时不降级。检查器改变审核证据后，在同一执行轮内创建新候选并重新只读审核，不重新执行任务或消耗额外执行轮。已知通过结果绑定检查器返回时的证据版本；版本仍匹配时复用该结果，避免重复执行检查器，证据再变化则失效。

`Human` 在独立审核通过（及配置的检查器通过）后进入 blocked/verifying 等待宿主 `approve_goal`。批准必须提供非空人工说明，并重新核对候选、证据、检查器版本及绝对期限；过期批准拒绝且不修改目标。basis 为 `human_approved`，不能绕过配置的检查器。历史 `model_reported` basis 仅用于旧已结束目标展示。

文件证据限工作目录内的规范路径，artifact://ID 限会话产物。审核者新生成并实际读取的 artifact 经宿主 ArtifactManager 导入父会话并重绑定引用，字节必须与读取时的 SHA-256 一致；继承产物保留身份，不能以子会话副本掩盖父证据变化或缺失。保存每份实际读取证据的 SHA-256；完成提交核对 run、目标身份、契约版本、conversation 序号、取消状态与期限。审核期间保持 turn gate 与宿主容量，任务实际退出前不释放。迟到结果不能改变停止或替换后的目标。

检查器开始前持久保存 pending marker；已知结果先与 checker_records 同次发布，再关闭 marker。即使返回后证据读取失败，已知结果也保持已知并关闭 marker，另以证据错误阻塞，不能伪装成未知副作用。异常或未知结果保留 marker，恢复后必须人工调查并 `reconcile_goal_effects`，不能自动重放未知副作用或合成成功。一般有副作用工具也保留既有 marker 协议，Bash/远端操作不承诺 exactly-once。

## 读取、缓存与恢复

GoalRead 无必填参数，返回 `{ goal, draft }`，当前草案单独置于 draft；无目标为两个 null。结果包含完整契约、版本、预算、阶段、requirement_ids、latest_round_decision、完整审核与轻量继续历史，通过统一工具输出保护，过长内容落 artifact。它不提供确认或生命周期写权限。

确认后的首轮在尾部携带稳定契约与动态 round 事实；后续轮尾部只追加当前轮状态和审核反馈。压缩后挂载稳定契约 checkpoint，轮数/阶段/决策不进入前缀；契约被有损省略时明确提示 GoalRead。system/tools 不随目标状态改变，审核者有独立固定工具面，不承诺与主代理共享完整工具前缀缓存。conversation 只追加，不重写历史。

持久化容器、快照与契约都带格式编号，未知编号拒绝读取为可执行状态。旧候选恢复时继续完整审核，新增记录与计数默认空，不重置额度。工具面或工作目录变化会阻止恢复并要求人工对账，不做自动迁移。成功报告与内容 SHA-256、证据版本、引用映射和历史水位持久化；宿主提交失败后可复用报告，避免重跑已结束任务。重复继续引起的升级在续审前持久化强制完整审核标志；进程重启后可新建审核子会话继承有效材料。轻量记录与调度决策同次原子发布，恢复不重报、不重复累计。

恢复保留已用额度与 pending effects：额度不足先由宿主明确调整，未知副作用先对账。未知持久化版本拒绝读取为可执行状态。

core SessionLease 在恢复前获取；停止意图发布失败进入 Stopping 并中断，实际执行退出后才 Faulted；不能按磁盘旧 active 自动重启。正常 shutdown 保留已确认 active 意图供恢复。Rust 默认不自动驱动，终端在 UI 接入后接起已确认 active；server 只有显式确认或 resume 才启动带容量的 run。未完成只读审核可针对仍有效候选重启，历史审核结果可幂等复用。

## 宿主接口与界面

Rust：draft_goal、request_goal_draft、goal_draft_snapshot、generate_goal_draft、generate_goal_draft_at、confirm_goal、confirm_and_start_goal、cancel_goal_draft；`generate_goal_draft_at` 在同一准入锁内核对 ID/revision 并使旧草案不可确认，陈旧修改在写入和模型请求前拒绝。GoalStore 通过类型化 GoalCommand 统一处理草案创建、替换、确认与取消。保留 goal_snapshot、subscribe_goal、pause/resume/cancel、预算调整、人工批准和副作用对账。旧 create_goal/create_and_start_goal 明确返回协议升级错误。`start_goal_driver` 只启用调度；有容量账本的宿主经 confirm_and_start_goal/resume_and_start_goal 传递 GoalRunLease，执行和退出期间持续占容量，等待人工验收时释放。

Web：GET `/api/sessions/{id}/goal` 优先读活运行时快照，关闭时回退落盘 current；hidden 归一为 null。POST 同路径要求 `protocol_version:2`，objective、acceptance（agent_reviewed/human/verifier）、max_turns、max_minutes、acceptance_criteria、constraints；默认形成模型草案，宿主准备结构化 `draft_spec` 时可设 `generate:false`。成功返回 draft，尚未启动执行。

POST `.../goal/{action}` 必须携带 goal_id/revision；新增 confirm、refine（feedback），保留 pause/resume/cancel/clear/budget/approve/reconcile。确认预约容量并由 core 激活；refine 的模型讨论也占同一宿主容量。SSE goal_updated 与镜像水位保持同一发布边界；已清除（hidden）的目标在事件、镜像与快照中同样归一为 null，草案和审核详情只呈现 core 快照。

CLI/TUI：`/goal <请求>`、`/goal refine <意见>`、`/goal confirm <ID> <REVISION>`；/goals 与可点击状态标记显示只读详情；`/goal` help 与解析分支列出同一组子命令（含 reconcile），`/goal confrim`、`/goal refne` 等误拼在三个解析器一致拒绝并给出建议。草案讨论通过类型化内部消息持久化，携带原始请求与修改意见，不注册为人类 Inbox 请求。Web 显示待确认草案卡片，控制指令只进入模型上下文。界面区分执行轮 Final 与 GoalFinished，呈现待确认、执行中、审核中、待人工验收及逐项审核证据；子目标进度仅由同一契约版本的最新审核派生；当前候选存在时必须匹配该候选。Python SDK 没有 Goal 产品入口。
