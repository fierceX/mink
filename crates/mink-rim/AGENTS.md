# Microkernel Rim Invariants (`mink-rim`)

> 归属：`crates/mink-rim/`；更新日期：2026-11-28；架在 `mink-core` 极简可逆接缝之上的可选声明式外环微内核 crate。

## 关键不变式

- **零反向耦合**：`mink-core` 对 `mink-rim` 零依赖；`mink-rim` 仅通过 `mink::runtime` 与 `mink::runtime::ext` 公开接缝装配和撤销扩展（由 `crates/mink-core/tests/public_api_boundary.rs` 守护）。
- **实例级所有权与作用域隔离**：禁止进程全局单例；`RimSession` 与根 `Scope` 绑定到单个 `AgentRuntime` 的 `on_closing`（排空前联动 `root_scope.close()`）与 `teardown` 栈；`Scope::try_fork` 并发下保证继承父 closing；`RimSession` 区分 `root_scope`（`""`，全会话含子代理）、`primary_scope` / `fork_scope`（`"primary/..."`，仅主代理）与 `try_fork_subagent_scope`（`"subagent/<id>"`，仅该子代理及其后代），并结合 `scopes_related` 实现挂点隔离。
- **零残留与 LIFO 撤销**：`Scope::dispose()` 在锁外按严格 LIFO 顺序幂等释放所有 `Disposer`；已释放作用域上的迟到注册立即撤销；`try_push_labeled` 在 closing/disposed 时原子回滚并拒绝挂入；子作用域提前释放后父作用域通过 `spent_when` 自动剪枝。
- **三态显式仲裁与 Fail-Closed 去重**：`SeatRegistry<P>` 只返回 `Ok(Arc<P>)` 或 `UnknownProvider` / `Unavailable` / `Ambiguous`，禁止静默降级或盲猜；重复同 ID 注册返回 `SeatError::DuplicateProvider`，关闭态作用域返回 `SeatError::ScopeClosed`；`SeatHub` 在 `RimInstallContext` 与 `RimSession` 间共享并在 `resolve_seat` 中遵循 `seat_overrides`。
- **启动原子回滚与无 Panic**：`RuntimeAssembler::apply_profile` / `apply_profile_with_seats` / `start_runtime` 在未知 bundle、重复启用同 ID bundle、非法 `bundle.id()`（通过 `try_fork` 返回 `Err`，绝不 panic）或任意 `RimBundle` 安装/启动失败时立即触发已创建 `Scope` 的 LIFO 回滚（fail-closed）；`RimBundle` 负责声明式 patch 装配与回滚，`mink-core::runtime::RuntimePlugin` 仅作无配置纯 Rust 快捷方式。
