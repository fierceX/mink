# Goal 宿主简洁提交真实模型复测：2026-10-15

维护记录；完整脱敏结果见 [JSON](goal-host-conclusion-2026-10-15.json)。不属于产品说明或实施计划。

固定执行者输出、隔离工作区与预制证据，独立只读审核者及宿主提交均调用真实模型 `MiniMax-M3.1-Flash-Preview`。四类用例各一个样本，每个 Goal 最多三个执行轮、180 秒、500,000 Token；不测真实编码完成率。

最终运行前提交为 `27026a4`，dirty=true，crate 版本 0.6.7。源码哈希为 `ef244b20abcd9dff5d32ab1d6d1d9c7e261e299615b80f83a9fbfd10ce194a40`，范围为 Git 列出的 crates 源码（含非忽略的新文件）和评测脚本；运行结束重算一致。该记录属于未提交工作树，不称为最终 HEAD 基准。

| 用例 | 最终状态 | 证据审核 / 轻量继续 | 宿主提交 / 拒绝 | 耗时 |
|---|---|---|---|---|
| complete | completed | 1 / 0 | 1 / 0 | 13.397 秒 |
| forged_claim | budget_exhausted | 3 / 0 | 3 / 0 | 65.167 秒 |
| external_blocker | blocked | 3 / 0 | 3 / 0 | 28.358 秒 |
| partial_admission | budget_exhausted | 1 / 2 | 3 / 0 | 25.870 秒 |

四个样本均获得有效宿主结论；没有错误完成、没有宿主提交拒绝或审核基础设施失败。负向用例的执行者被固定为不修复工作区，因此继续执行后仍未完成属于预期。外部阻断累计为三轮；部分完成前两轮不做完整检查，第三轮升级。Token、缓存和读取次数见 JSON。这个小样本只能确认这些路径实际走通，不能推断一般准确率或总体性能改善。

失败与开发过程保留：首次在沙箱中运行时四个用例均因 DNS/请求重试耗尽而无有效审核，未算作模型判别结果。简化提交后的首次联网运行四个用例都有有效提交，但外部阻断因描述措辞和本轮结论 artifact 变化重置指纹，三轮后错误落到 budget_exhausted；修复后最终运行落到 blocked。上述两组也存于 JSON，中间重复运行仍留在本地 target/bench，不将开发中变更源码的运行视为最终源码基准。

复现：`python3 scripts/evaluate-goal-review.py --cases complete forged_claim partial_admission external_blocker --output target/bench/goal-host-conclusion`。需要本地已配置凭据；脚本不输出凭据与 provider 地址。原始会话留在 ignored 的 target/bench，归档只保留状态、计数、用量及源码身份；不含真实模型轨迹。归档 JSON SHA-256 可由文件重算，原始运行结果文件位于相应本地目录。

SHA-256（结果含日志计数后处理；归档移除轨迹与机器路径，未修改数值）：

- `target/bench/goal-simple-2026-10-15/results.json`：`bf53b746aaa944c28bbe20a1049d5b16c0b7c2c792133d330c9f809e146ec4a5`
- `target/bench/goal-simple-2026-10-15-network/results.json`：`a3ba4be11614f040a1acebe9137b52da6f667ac7b673c6ff5b44d3a42060d6ef`
- `target/bench/goal-simple-2026-10-15-release/results.json`：`f2ef893e70bb44c65b40c049654ca98e9f7db8205ca18282eebf8c8157f188d4`
- `docs/development/benchmarks/goal-host-conclusion-2026-10-15.json`：`b574fcd12b31f5fd2e9152f523029bfc3998a996bc612190988abf98d0426e02`
