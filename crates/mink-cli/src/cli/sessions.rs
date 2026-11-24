use super::help::{pad_display, truncate_display};
use super::*;

pub(super) async fn read_session_alias(session: &crate::runtime::SessionInfo) -> Option<String> {
    let metadata_path = session.events_path.with_file_name("session.json");
    let text = tokio::fs::read_to_string(&metadata_path).await.ok()?;
    let value: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    value.get("alias")?.as_str().map(|s| s.to_string())
}

/// After all turns complete, auto-generate a session title from the first user
/// input if the session.json doesn't already have a title. This ensures
/// interactive/TUI sessions show a meaningful name in `--list-sessions`.
pub(super) async fn auto_set_session_title(session: &crate::runtime::SessionInfo) {
    let metadata_path = session.events_path.with_file_name("session.json");
    derive_and_persist_title(&metadata_path, &session.conversation_path).await;
}

/// 从 conversation.jsonl 首条真实 user 消息生成标题并原子写回 session.json。
/// 已有非空标题时不动。此前该逻辑在 auto_set_session_title 与
/// resolve_session_title 各实现一份且都是非原子写。
pub(super) async fn derive_and_persist_title(
    metadata_path: &Path,
    conversation_path: &Path,
) -> Option<String> {
    use tokio::io::AsyncBufReadExt as _;

    let metadata_text = tokio::fs::read_to_string(metadata_path).await.ok()?;
    let mut metadata: serde_json::Value = serde_json::from_str(&metadata_text).ok()?;
    if metadata
        .get("title")
        .and_then(|v| v.as_str())
        .is_some_and(|t| !t.is_empty())
    {
        return None;
    }
    let file = tokio::fs::File::open(conversation_path).await.ok()?;
    let mut reader = tokio::io::BufReader::new(file);
    let mut line = String::new();
    let title = loop {
        line.clear();
        let n = reader.read_line(&mut line).await.ok()?;
        if n == 0 {
            return None;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if msg.get("role").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }
        let Some(content) = msg.get("content").and_then(|v| v.as_str()) else {
            continue;
        };
        if let Some(title) = crate::session::metadata::title_from_prompt(content) {
            break title;
        }
    };
    metadata["title"] = serde_json::Value::String(title.clone());
    let now = time::OffsetDateTime::now_utc();
    let fmt = time::format_description::well_known::Rfc3339;
    metadata["updated_at"] = serde_json::Value::String(now.format(&fmt).unwrap_or_default());
    if let Ok(text) = serde_json::to_string_pretty(&metadata) {
        // Cosmetic title write-back: best-effort, never fails the run.
        let path = metadata_path.to_path_buf();
        let payload = format!("{text}\n");
        let _ = tokio::task::spawn_blocking(move || {
            let _ = mink::runtime::atomic_replace(&path, payload.as_bytes());
        })
        .await;
    }
    Some(title)
}

pub(super) fn list_skills() {
    crate::local::print_skills();
}

pub(super) async fn list_sessions(home: &Path, cwd: &Path) -> Result<()> {
    let mut rows = crate::session::metadata::list_project_sessions(home, cwd).await?;
    if rows.is_empty() {
        println!("No sessions found.");
        return Ok(());
    }
    rows.sort_by_key(|row| std::cmp::Reverse(row.modified));
    println!(
        "{} {} {} ID",
        pad_display("ALIAS", 20),
        pad_display("TITLE", 32),
        pad_display("UPDATED", 16),
    );
    let alias_width = 20;
    let title_width = 32;
    let updated_width = 16;
    for row in rows {
        let alias = row.alias.as_deref().unwrap_or("-");
        let title = resolve_session_title(&row.path, &row).await;
        let alias = truncate_display(alias, alias_width);
        let title = truncate_display(&title, title_width);
        let dt: time::OffsetDateTime = row.modified.into();
        let formatted = {
            use time::macros::format_description;
            static FMT: &[time::format_description::FormatItem<'_>] =
                format_description!("[year]-[month]-[day] [hour]:[minute]");
            dt.format(FMT)
                .unwrap_or_else(|_| format!("{:?}", row.modified))
        };
        println!(
            "{} {} {} {}",
            pad_display(&alias, alias_width),
            pad_display(&title, title_width),
            pad_display(&formatted, updated_width),
            row.id,
        );
    }
    Ok(())
}

/// Return the session title, lazily generating it from the first user
/// conversation turn when session.json has no title. If generated, only the
/// `title` field is written back to session.json — all other fields are
/// preserved untouched.
async fn resolve_session_title(
    session_dir: &Path,
    meta: &crate::session::metadata::SessionRecord,
) -> String {
    if let Some(title) = meta.title.as_deref().filter(|t| !t.is_empty()) {
        return title.to_string();
    }
    if let Some(summary) = meta.summary.as_deref().filter(|s| !s.is_empty()) {
        return summary.to_string();
    }
    // Lazy path: 生成并原子写回（与交互路径共用同一实现）。
    let metadata_path = session_dir.join("session.json");
    let conversation_path = session_dir.join("conversation.jsonl");
    derive_and_persist_title(&metadata_path, &conversation_path)
        .await
        .unwrap_or_else(|| "-".to_string())
}
