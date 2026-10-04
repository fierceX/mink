# 实跑案例：修复 .env 解析器，运行中补充约束

> 录制日期：2026-10-04 · Mink 0.6.6 · Full TUI · 模型别名 flash

这是为官网在独立目录准备并实际运行的小型 Rust 案例，使用已配置的真实模型后端。
用户输入通过 TUI 提交，工具实际修改文件并执行 `cargo test`。未使用模拟 LLM 响应。

## 任务与中途引导

**最初任务（原文）：**

> 修复这个 .env 解析器：值里包含等号时会被截断，还需要支持单双引号。先读代码，再修改并运行测试，用中文简洁回复。

**运行中引导（原文）：**

> 补充一个约束：只去掉首尾配对引号，不展开变量；空值也要保留，禁止新增依赖。

引导记录的 `_mink.guidance` 为 true，`turn_id` 与最初任务一致。
它位于首个工具结果之后，后续读取、编辑与验证使用了补充要求。

## 实际过程

1. 读取目录与 `Cargo.toml`、`README.md`、`src/lib.rs`。
2. 新增测试，第一次验证出现 3 项失败，覆盖等号截断与引号问题。
3. 将实现改为 `split_once('=')`，仅剥离首尾配对引号，不展开变量。
4. 再次验证，6 项测试全部通过；Cargo.toml 没有新增依赖。

```text
test tests::empty_value_preserved ... ok
test tests::no_assignment_is_none ... ok
test tests::simple_assignment ... ok
test tests::no_variable_expansion ... ok
test tests::strips_matching_quotes_only ... ok
test tests::value_may_contain_equals ... ok

test result: ok. 6 passed; 0 failed; 0 ignored
```

录制中的测试命令使用了 `cargo test 2>&1 | tail ...`，工具进程退出码为 0；
回放保留真实工具状态，同时保留第一次测试失败的正文。录制完成后另行直接执行
`cargo test` 确认结果，没有依赖管道退出码判断成功。

## 回放的取舍

回放由 `scripts/build_hero_replay.py` 从 conversation.jsonl 导出，每一步保留
`sourceMessage`。内部控制诊断不作为用户输入，长 thinking/回复节选，工具输出保留摘要或
测试行；终端控制码清洗，机器路径匿名化。输入动画按已提交的真实引导重建，播放节奏加速，
并非逐帧屏幕录像。底栏根据 events.jsonl 的 usage 重建已结算统计，通过调用 ID 或
完整回复正文关联正式消息，并保留 `statusSourceEvent` 作为来源行号。被废弃响应
依然计入账单用量；导出必须与 stats.json 的最终计数及 Token 分区核对。
未记录的中间信念值显示 `B:—`，起始 `B:0.75` 与结束 `B:0.92` 来自明确事件。

引导进入后实际已有一笔结算：`T:1 R:1 I:2.6k(4%) O:93 C:3.6k(0%)`。
最终回复打字期间保留上一笔 usage，完成后才展示最终计数，不提前填入未来统计。

此次录制的最终统计：1 轮用户任务、9 次主模型请求、44,477 输入 Token（含缓存读取）、
5,566 输出 Token，最终上下文 8,829 Token。底栏按真实 TUI 格式展示为
`I:44.5k(80%) O:5.6k C:8.8k(0%)`；缓存占比仅统计缓存读取，分母包含未缓存、
缓存读取和缓存创建。事件记录的上下文上限为 1,000,000 Token；百分比按 TUI
整数口径取整，因此此处显示 0%。这些是本案例的记录，不是性能基准。

## 修复后的代码

以下为实际工具写入的 `src/lib.rs`，可以放入独立的 Rust library 项目运行 `cargo test`。

```rust
/// Parse a key/value assignment from an .env file.
///
/// Splits on the first `=` so values may contain `=`. The value is trimmed,
/// then one layer of matching surrounding quotes (single or double) is
/// removed. No variable expansion or escape processing is performed, and
/// empty values are preserved.
pub fn parse_assignment(line: &str) -> Option<(String, String)> {
    let (key, raw_value) = line.split_once('=')?;
    let value = raw_value.trim();
    let value = match (value.as_bytes().first(), value.as_bytes().last()) {
        (Some(first), Some(last)) if value.len() >= 2 && first == last && (*first == b'"' || *first == b'\'') => {
            &value[1..value.len() - 1]
        }
        _ => value,
    };
    Some((key.trim().to_string(), value.to_string()))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn simple_assignment() {
        assert_eq!(parse_assignment("NAME=mink"), Some(("NAME".into(), "mink".into())));
    }
    #[test]
    fn value_may_contain_equals() {
        assert_eq!(parse_assignment("PATH=/usr/bin=extra"), Some(("PATH".into(), "/usr/bin=extra".into())));
        assert_eq!(parse_assignment("KEY==v"), Some(("KEY".into(), "=v".into())));
    }
    #[test]
    fn strips_matching_quotes_only() {
        assert_eq!(parse_assignment("GREETING=\"hello world\""), Some(("GREETING".into(), "hello world".into())));
        assert_eq!(parse_assignment("MSG='a = b'"), Some(("MSG".into(), "a = b".into())));
        assert_eq!(parse_assignment("KEEP=\" spaced \""), Some(("KEEP".into(), " spaced ".into())));
        assert_eq!(parse_assignment("MIXED=\"abc'"), Some(("MIXED".into(), "\"abc'".into())));
        assert_eq!(parse_assignment("LONE='"), Some(("LONE".into(), "'".into())));
        assert_eq!(parse_assignment("QUOTED_EMPTY=\"\""), Some(("QUOTED_EMPTY".into(), "".into())));
    }
    #[test]
    fn no_variable_expansion() {
        assert_eq!(parse_assignment("RAW=\"$HOME\""), Some(("RAW".into(), "$HOME".into())));
    }
    #[test]
    fn empty_value_preserved() {
        assert_eq!(parse_assignment("EMPTY="), Some(("EMPTY".into(), "".into())));
        assert_eq!(parse_assignment("PAD=   "), Some(("PAD".into(), "".into())));
    }
    #[test]
    fn no_assignment_is_none() {
        assert_eq!(parse_assignment("JUST_A_KEY"), None);
    }
}
```
