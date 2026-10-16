# Goal 验收边界修复后复测：2026-10-12

维护测量，不进入官网发布产物。数据见 [脱敏 JSON](data/goal-acceptance-fixes-2026-10-12/results.json)。

## 来源与方法

版本 0.6.7，基点 `46b5d5a6c3b7844cb4fac5adb9081b0c20f8e955`，两个运行均为 dirty 修复快照。运行前捕获源码 SHA-256（范围见 JSON）：审核 `0827d0787b53344f6dfe8ae830e7e72c4aaffac36ea5d08407863941d0b13ca0`，草案 `92009fd678cdc8cf29c4ff2b65436e8c886bbdbcbdec5dde8c4a565e61fcda76`。六样本运行开始后仅补齐 server 测试 fixture 的新增快照字段；审核运行时代码未再变化，最终草案运行捕获包含该修正的源码。不得把这些 dirty 测量称为提交后 HEAD 基准。

模型为 `MiniMax-M3.1-Flash-Preview`，按运行时现有配置调用。固定执行者每轮只声称完成，真实只读审核者核对预制工作区。每类一个样本、最多三个执行轮；外部阻断样本另有明确的 operator-status.json 事实记录供 Read 检查。它测审核判别与预算门槛，不测编码成功率，也不与历史其他模型样本直接比较。

## 六个样本

| 用例 | 终态 | 执行轮 / 有效审核 | 整段耗时 ms | 非缓存输入 / 缓存读取 / 输出 Token | 缓存读取比例 |
|---|---|---:|---:|---:|---:|
| complete | completed | 1 / 1 | 20467 | 8315 / 22528 / 1861 | 73.04% |
| partial | budget_exhausted | 3 / 3 | 46786 | 25288 / 53095 / 4265 | 67.74% |
| forged_claim | budget_exhausted | 3 / 3 | 55609 | 50294 / 75387 / 5096 | 59.98% |
| missing_evidence | budget_exhausted | 3 / 3 | 69535 | 22786 / 88944 / 6933 | 79.61% |
| external_blocker | blocked | 3 / 3 | 60956 | 21916 / 66753 / 4188 | 75.28% |
| unverifiable | budget_exhausted | 3 / 3 | 62805 | 38721 / 72122 / 7365 | 65.07% |

六个样本均产生有效审核；完整证据完成，四种未完成情况停止于预算，外部阻断在三轮核对后 blocked。本组误判完成 0、误拒绝 0、无效或失败样本 0。正例只有一个，不能推断一般准确率改善。

延迟为整个 Goal run，包含审核、工具读取、纠错、调度与模拟执行；独立审核请求延迟、P50/P95 未单独测量。模拟执行无 provider Token，仍记录 unreported 请求；表中 Token 为此次审核开销，未测无审核执行者对照，不能称为端到端新增 Token 差值。缓存比例分母含 input/read/creation，creation 此次为 0。

## 两轮草案

真实模型两轮讨论含一次修改意见，耗时 89137 ms。两次草案 ready，契约有效、原始请求保留、仍为 draft，执行轮数 0，工作区不变。草案 Token 分项未采集。这确认本样本的确认边界，不证明所有目标拆解都无遗漏。

## 验证边界

修复后的 Rust 验证：core 1105、CLI 264、server 64 项单测通过，相关集成测试通过；fmt、全 workspace/all-targets/all-features clippy 与 HTTP/SSE Goal 回归通过。最初全 Rust 编译发现 server fixture 缺新增字段，补齐后重跑通过；不隐去该失败。Web 前端测试依用户要求未运行。构建 feature 配置未改变，未额外运行 feature-matrix。

复现：`python3 scripts/evaluate-goal-review.py --output target/bench/goal-review`；草案追加 `--draft-only`。原始会话、凭据配置和诊断留在本地 ignored 目录。此页只记录实现后的行为与测量，不包含实施计划或临时审计报告。

归档 JSON SHA-256：`9389d7df073e3752e26cd3bc85ee6ae32255e43973495c78dcc6f8417ba019a8`。
