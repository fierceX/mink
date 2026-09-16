use once_cell::sync::Lazy;
use regex::Regex;
use std::path::{Path, PathBuf};

static RE_FIND_DELETE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(^|[;&|])\s*find\b.*\bdelete\b").expect("regex"));
static RE_FORK_BOMB: Lazy<Regex> = Lazy::new(|| Regex::new(r":\(\)\{:\|:&\};:").expect("regex"));
static RE_BLOCK_DEVICE_WRITE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(^|\s)(of=|>|1>|>>|1>>)\s*/dev/(sd[a-z][0-9]*|disk[0-9]+|rdisk[0-9]+|nvme[0-9]+n[0-9]+(p[0-9]+)?|vd[a-z][0-9]*|xvd[a-z][0-9]*|hd[a-z][0-9]*)(\s|$)")
        .expect("regex")
});
/// `rm -rf` / `rm -fr` 风格调用的目标段（到 shell 分隔符为止）。
/// 判定链：受限 POSIX 词法解析（转义/引号）→ symlink 感知的真实路径解析
/// （`realpath(strict=False)` 语义）→ 临时目录白名单；任何无法静态确定
/// 的目标（命令替换/变量/字符类/花括号/重定向/未闭合引号）保守拦截。
static RE_RM_RECURSIVE_FORCE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\brm\s+-[a-z]*r[a-z]*f[a-z]*\s+([^\n;&|]+)").expect("regex"));
static RE_RM_FORCE_RECURSIVE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\brm\s+-[a-z]*f[a-z]*r[a-z]*\s+([^\n;&|]+)").expect("regex"));

/// 删除属于日常清理的临时目录；其余绝对路径保持原有拦截力。
const TEMP_DELETE_PREFIXES: [&str; 4] = [
    "/tmp",
    "/var/folders",
    "/private/tmp",
    "/private/var/folders",
];

/// symlink 解析与段数上限（防病态输入）。
const MAX_RESOLVED_COMPONENTS: usize = 64;
const MAX_SYMLINK_HOPS: usize = 40;

/// 受限 POSIX 词法扫描：把目标段切成词，处理 `\` 转义与单/双引号包裹。
/// 返回 None 表示无法静态确定（未闭合引号、命令替换/变量、字符类、花括号、
/// 重定向，或双引号内出现转义）。
fn split_shell_words(segment: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut chars = segment.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' => loop {
                match chars.next() {
                    Some('\'') => break,
                    Some(other) => current.push(other),
                    None => return None,
                }
            },
            '"' => loop {
                match chars.next() {
                    Some('"') => break,
                    Some('\\') | Some('$') | Some('`') => return None,
                    Some(other) => current.push(other),
                    None => return None,
                }
            },
            '\\' => match chars.next() {
                Some('\n') => {}
                Some(other) => current.push(other),
                None => return None,
            },
            '$' | '`' | '[' | '{' | '<' | '>' => return None,
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            other => current.push(other),
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    Some(words)
}

/// 解析绝对路径的真实形态（symlink 感知），语义对齐 `realpath(strict=False)`：
/// 不存在的尾段按词法继续（`.`/`..` 仍生效）。None = 不可判定（非绝对路径、
/// IO 异常、超过跳数/段数上限）。
fn resolve_real_path(path: &str) -> Option<PathBuf> {
    if !path.starts_with('/') {
        return None;
    }
    let mut pending: std::collections::VecDeque<String> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect();
    let mut stack: Vec<String> = Vec::new();
    let mut hops = 0usize;
    let mut unresolved = false;
    while let Some(segment) = pending.pop_front() {
        if stack.len() + pending.len() > MAX_RESOLVED_COMPONENTS {
            return None;
        }
        match segment.as_str() {
            "." => continue,
            ".." => {
                stack.pop();
                continue;
            }
            _ => {}
        }
        if unresolved {
            stack.push(segment);
            continue;
        }
        let mut candidate = PathBuf::from("/");
        for part in &stack {
            candidate.push(part);
        }
        candidate.push(&segment);
        match std::fs::symlink_metadata(&candidate) {
            Ok(meta) if meta.file_type().is_symlink() => {
                hops += 1;
                if hops > MAX_SYMLINK_HOPS {
                    return None;
                }
                let target = std::fs::read_link(&candidate).ok()?;
                if target.is_absolute() {
                    stack.clear();
                }
                let target_segments = target
                    .to_string_lossy()
                    .split('/')
                    .filter(|part| !part.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                for part in target_segments.into_iter().rev() {
                    pending.push_front(part);
                }
            }
            Ok(_) => stack.push(segment),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // 尾部不存在：按词法继续。
                unresolved = true;
                stack.push(segment);
            }
            Err(_) => return None,
        }
    }
    let mut resolved = PathBuf::from("/");
    for part in stack {
        resolved.push(part);
    }
    Some(resolved)
}

fn is_temp_delete_target(word: &str) -> bool {
    let Some(resolved) = resolve_real_path(word) else {
        return false;
    };
    let lowered = resolved.to_string_lossy().to_ascii_lowercase();
    TEMP_DELETE_PREFIXES
        .iter()
        .any(|prefix| Path::new(&lowered).starts_with(prefix))
}

/// 返回第一个应被拦截的目标（绝对路径且真实形态不在临时白名单内）；
/// `None` 表示该命令的所有 `rm -rf` 目标都可放行。词法不可确定的段返回
/// 占位标记，保证调用方 fail closed。
fn destructive_rm_verdict(command: &str) -> Option<String> {
    for pattern in [&*RE_RM_RECURSIVE_FORCE, &*RE_RM_FORCE_RECURSIVE] {
        for capture in pattern.captures_iter(command) {
            let Some(segment) = capture.get(1) else {
                continue;
            };
            let Some(words) = split_shell_words(segment.as_str()) else {
                return Some("<unresolvable shell target>".to_string());
            };
            for word in words {
                if !word.starts_with('/') {
                    // 相对路径、`~` 与裸变量目标维持既有放行行为（旧规则
                    // 同样不识别它们）。
                    continue;
                }
                if !is_temp_delete_target(&word) {
                    return Some(word);
                }
            }
        }
    }
    None
}

pub fn deny_bash_command_reason(command: &str) -> Option<&'static str> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Some("empty command");
    }

    let lower = trimmed.to_lowercase();
    for p in [
        "sudo ", "shutdown", "reboot", "halt", "poweroff", "mkfs", "fdisk",
    ] {
        if lower.starts_with(p) {
            return Some("dangerous command prefix");
        }
    }
    if destructive_rm_verdict(&lower).is_some() {
        return Some("destructive root delete pattern");
    }
    if RE_FIND_DELETE.is_match(trimmed) {
        return Some("blocked destructive find -delete pattern");
    }
    if RE_FORK_BOMB.is_match(trimmed) {
        return Some("fork bomb pattern");
    }
    if RE_BLOCK_DEVICE_WRITE.is_match(trimmed) {
        return Some("block device write pattern");
    }
    None
}

#[cfg(test)]
#[path = "safety_tests.rs"]
mod tests;
