# Goal 发布构建效果验证：2026-10-09

维护记录；结论只适用于下面这一次构建与这两个小样本，不是整体成功率声明。

## 结论

发布构建 `c94b52805032e8f2a292964fddad27cc3453d321`（dirty=false）在真实 provider 上跑通两条浏览器端到端用例：正常完成目标推进 2 个轮次并以 `model_reported` 完成；两轮预算下目标同样在预算内完成。两条都证明调度跨轮推进，而非在一轮内循环——轮次数来自目标的权威快照，不是模型请求计数。

这组样本不证明编码任务成功率、首次验收拒绝后的续跑质量或长期稳定性；此前 2026-10-09 的 `c5d4d71` 样本记录了报告被拒与预算耗尽的失败案例，本记录不覆盖那些路径。

## 环境与方法

- 源码 `c94b52805032e8f2a292964fddad27cc3453d321`，dirty=false，Mink 0.6.7；macOS、release 构建。
- `target/release/mink-server` SHA-256 `8dba5b6afb28c69483573c3c34d7000af6646057e6b6728d09be6eb01eabe101`；前端资产由该构建的 `web/dist` 嵌入。
- 使用 `~/.minkrc` 中已配置的 provider（`api.minimax.cn`，模型别名 `flash` 解析为 `MiniMax-M3.1-Flash-Preview`）。密钥未打印、未写入本目录。
- 复跑：`MINK_E2E_REAL=1 npx playwright test goal-real-rounds`（需本机回环监听与真实网络）。

## 观测

| 场景 | 轮次 | 终态 | 完成依据 |
|---|---|---|---|
| 正常完成（两个交付物） | 2 | completed | model_reported |
| 两轮预算上限 | 2 | completed | model_reported |

## 边界与未知

- 只覆盖工作台的目标控制、卡片渲染与状态标记；未测并发创建、初始快照恢复与移动端（这些由可控后端回归覆盖，不消耗真实调用）。
- 未记录 token 与费用：服务端 usage journal 记录了请求，但本记录不汇总用量。
- 失败样本：本次两条用例均通过，没有可保留的失败样本；这不是成功率的正面证据。
