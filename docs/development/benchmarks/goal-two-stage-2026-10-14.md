# Goal 两阶段审核真实模型记录：2026-10-14

维护记录；结果仅适用于合成小样本。完整脱敏数据见 [JSON](goal-two-stage-2026-10-14.json)。

## 方法与源码身份

固定执行者给出完成、部分完成、模糊或矛盾声明；文件在隔离临时工作区预制。独立只读审核者及宿主提交均调用真实模型。每组十类任务各一个样本，最多三个执行轮。它检验分流、证据审核及提交协议，不测真实编码完成率。

完整策略组在同一开发 runtime 的临时请求尾部强制要求证据检查和 full 提交；两阶段组使用实际默认行为。它不是旧提交二进制对照。两组并行执行，provider 缓存及采样有波动，不能据此推断总体效果。

模型：`MiniMax-M3.1-Flash-Preview`；crate 版本 `0.6.7`。运行前 Git 提交为 `8254233685164ea488ca9d7f57bc4b8845dc0273`，dirty=true。每组 JSON 保存各自启动时间、源码 SHA-256 与 source_scope；后续日志计数另记 postprocessing_source_sha256。这些是实现过程中的开发快照，不称为提交后的 HEAD 实测。

复现：`python3 scripts/evaluate-goal-review.py --output target/bench/goal-review`；完整策略追加 `--force-full`。请求真实模型需要本地配置或环境中的凭据。原始会话与日志留在 ignored 的 target/bench，归档不包含轨迹、用户目录、凭据或 provider 地址。Read 统计按正式工具调用记录计数，包括失败请求，不等于成功读取次数或唯一文件数。

## 结果

初次每任务 100,000 Token 额度导致多数未完成任务在审核期间耗尽；两组各八个无效或未完成审核样本，保留在 JSON 中。仅提高评测专用额度到 500,000 后重测，产品默认额度未改。

| 用例 | 两阶段状态 | 完整审核/轻量继续 | 完整策略状态 | 完整审核/轻量继续 |
|---|---|---|---|---|
| complete | completed | 1/0 | completed | 1/0 |
| partial | budget_exhausted | 3/0 | budget_exhausted | 3/0 |
| forged_claim | blocked | 0/0 | budget_exhausted | 3/0 |
| missing_evidence | budget_exhausted | 3/0 | budget_exhausted | 3/0 |
| external_blocker | budget_exhausted | 3/0 | budget_exhausted | 3/0 |
| partial_admission | budget_exhausted | 1/2 | budget_exhausted | 3/0 |
| ambiguous | budget_exhausted | 3/0 | budget_exhausted | 3/0 |
| optional_next_step | completed | 1/0 | completed | 1/0 |
| contradictory | budget_exhausted | 3/0 | budget_exhausted | 3/0 |
| unverifiable | budget_exhausted | 3/0 | budget_exhausted | 3/0 |

提高额度后的两阶段组九个有效样本，完整策略组十个；有效样本中的误判完成与误拒绝均为零。两阶段 forged_claim 在宿主提交轮达到轮次上限，没有有效最终结论，保留为失败样本；没有把它算作成功拒绝。该任务没有被错误完成。

partial_admission 的两阶段结果是两次轻量继续加一次完整检查；完整策略为三次完整检查。工作区 Read 请求分别为 2（审核者 1、宿主 1）和 7（审核者 4、宿主 3）。总 Read 请求含 artifact 分别为 3/7，Glob 为 2/6；模型请求数为 22/20，耗时 44.409/41.664 秒。两阶段的 input/cache-read/output Token 为 47,786/250,490/6,516，完整策略为 52,127/150,939/6,953；cache creation 均为 0。

该样本证实可以减少文件检查，但没有显示时间或总 Token 的改善。每轮启动审核者、宿主提交、第三轮升级与长上下文仍有成本；不同用例和 provider 缓存状态会影响结果。不可宣称两阶段已经提高一般准确率或整体性能。

自动化回归另外验证了：明确部分完成可无工作区工具调用地继续、三轮重复升级复用同一个审核子会话、提交拒绝同轮纠正、报告重开会话后复用、旧 schema 的精确迁移、预算与候选边界，以及新版 Web 完整审核/失败重试/轻量报告入口。
