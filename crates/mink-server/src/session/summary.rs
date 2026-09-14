//! Session list summary and usage aggregation (extracted from `registry.rs`).

use mink::runtime::session as runtime_session;
use std::path::Path;

#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionSummary {
    pub project_key: String,
    pub corrupt: bool,
    pub id: String,
    pub alias: Option<String>,
    pub title: Option<String>,
    pub cwd: String,
    pub created_at: String,
    pub updated_at: String,
    pub modified_secs: Option<u64>,
    /// Server-side runtime state: free (disk only) | active | running.
    pub status: &'static str,
    pub path: String,
    /// Usage 汇总（usage.jsonl）：会话累计 tokens，无记录时为 0。
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_read_tokens: u64,
    /// 最近一次请求的上下文估计（usage.jsonl 最后记录 input+cache），0 表示无记录
    pub last_context_tokens: u64,
}

/// 逐行读取 usage.jsonl 并汇总 tokens。缺失表示尚无用量；单个会话的
/// usage 读取错误（包括 I/O 级损坏）降级为零，避免拖垮整个会话列表。
pub(crate) fn summarize_usage(dir: &Path) -> (u64, u64, u64, u64) {
    match runtime_session::SessionReader::new(dir).usage_snapshot() {
        Ok(usage) => (
            usage.summary.tokens.input_tokens,
            usage.summary.tokens.output_tokens,
            usage
                .summary
                .tokens
                .cache_read_tokens
                .saturating_add(usage.summary.tokens.cache_creation_tokens),
            usage.last_context_tokens,
        ),
        Err(error) => {
            eprintln!(
                "[mink-server] warning: failed to summarize usage for {}: {error:#}",
                dir.display()
            );
            (0, 0, 0, 0)
        }
    }
}
