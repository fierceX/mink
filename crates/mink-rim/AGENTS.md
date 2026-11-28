# Microkernel Rim Invariants (`mink-rim`)

> 归属：`crates/mink-rim/`；更新日期：2026-11-28；架在 `mink-core` 极简可逆接缝之上的可选声明式外环微内核 crate。

## 关键不变式

- **零反向耦合**：`mink-core` 对 `mink-rim` 零依赖；`mink-rim` 仅通过 `mink::runtime` 与 `mink::runtime::ext` 公开接缝装配和撤销扩展（由 `crates/mink-core/tests/public_api_boundary.rs` 守护）。
- **实例级所有权**：禁止进程全局单例；`RimSession` 与根 `Scope` 绑定到单个 `AgentRuntime` 的 teardown 栈。
- **零残留与 LIFO 撤销**：`Scope::dispose()` 在锁外按严格 LIFO 顺序幂等释放所有 `Disposer`；已释放作用域上的迟到注册立即撤销；子作用域提前释放后父作用域通过 `spent_when` 自动剪枝。
- **三态显式仲裁**：`SeatRegistry<P>` 只返回 `Ok(Arc<P>)` 或 `UnknownProvider` / `Unavailable` / `Ambiguous`，禁止静默降级或盲猜；`Disposer` 通过单调递增 token 防止误删同名后继实现。
- **启动原子回滚**：`RuntimeAssembler::apply_profile` / `start_runtime` 在任意 `RimBundle` 安装或启动失败时触发已创建 `Scope` 的 LIFO 回滚（fail-closed）。
