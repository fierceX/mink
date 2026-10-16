use crate::capabilities::CapabilityWarning;
use crate::capabilities::source::{CapabilityExposure, SourceLevel, SourceMeta};
use anyhow::{Result, anyhow, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct LoadContext<'a> {
    pub cwd: &'a Path,
    pub home: &'a Path,
}

#[derive(Debug, Clone)]
pub struct SkillCapability {
    pub name: String,
    pub description: String,
    pub content: String,
    pub base_dir: String,
    pub disable_model_invocation: bool,
}

#[derive(Debug, Clone)]
pub struct LoadedSkill {
    pub skill: SkillCapability,
    pub source: SourceMeta,
    pub exposure: CapabilityExposure,
    pub revision: String,
}

impl LoadedSkill {
    /// Stable display label for the skill's source level, shared by the REPL
    /// and TUI so both surfaces report the same provenance.
    pub fn source_label(&self) -> &'static str {
        match self.source.level {
            crate::capabilities::SourceLevel::Runtime => "runtime",
            crate::capabilities::SourceLevel::Project => "project",
            crate::capabilities::SourceLevel::User => "user",
            crate::capabilities::SourceLevel::BuiltIn => "built-in",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub base_dir: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedSkill {
    pub info: SkillInfo,
    pub content: String,
}

pub trait SkillProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    fn priority(&self) -> i32;
    fn load_skills(&self, ctx: &LoadContext<'_>) -> Result<Vec<LoadedSkill>>;
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SkillDiscoveryPolicy {
    #[default]
    Defaults,
    RuntimeOnly,
    ExplicitOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSkill {
    pub name: String,
    pub description: String,
    pub content: String,
    pub exposure: CapabilityExposure,
    pub revision: Option<String>,
}

impl RuntimeSkill {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            content: content.into(),
            exposure: CapabilityExposure::ModelDiscoverable,
            revision: None,
        }
    }

    pub fn with_exposure(mut self, exposure: CapabilityExposure) -> Self {
        self.exposure = exposure;
        self
    }

    pub fn with_revision(mut self, revision: impl Into<String>) -> Self {
        self.revision = Some(revision.into());
        self
    }

    pub fn with_optional_revision(mut self, revision: Option<String>) -> Self {
        self.revision = revision;
        self
    }
}

#[derive(Debug, Clone, Default)]
pub struct SkillSnapshot {
    pub all: Vec<LoadedSkill>,
    pub discoverable: Vec<LoadedSkill>,
    pub selected: Vec<ResolvedSkill>,
    pub by_name: BTreeMap<String, LoadedSkill>,
    pub warnings: Vec<CapabilityWarning>,
    pub dependency_fingerprint: String,
}

pub fn build_default_skill_snapshot(
    cwd: &Path,
    home: &Path,
    selected_skills: &[String],
) -> Result<SkillSnapshot> {
    let providers = default_skill_providers();
    SkillSnapshot::load(&providers, &LoadContext { cwd, home }, selected_skills)
}

pub fn default_skill_providers() -> Vec<Arc<dyn SkillProvider>> {
    vec![
        Arc::new(RuntimeSkillProvider::default()),
        Arc::new(FileSystemSkillProvider::new(
            "project-claude-skills",
            "project .claude/skills",
            SourceLevel::Project,
            220,
            SkillBase::ProjectClaude,
        )),
        Arc::new(FileSystemSkillProvider::new(
            "project-local-skills",
            "project skills",
            SourceLevel::Project,
            210,
            SkillBase::ProjectLocal,
        )),
        Arc::new(FileSystemSkillProvider::new(
            "user-claude-skills",
            "user .claude/skills",
            SourceLevel::User,
            120,
            SkillBase::UserClaude,
        )),
        Arc::new(BuiltInSkillProvider),
    ]
}

pub fn skill_providers_for_policy(
    policy: SkillDiscoveryPolicy,
    runtime_skills: &[RuntimeSkill],
    explicit_providers: &[Arc<dyn SkillProvider>],
) -> Vec<Arc<dyn SkillProvider>> {
    let mut providers: Vec<Arc<dyn SkillProvider>> = Vec::new();
    providers.push(Arc::new(RuntimeSkillProvider::from_runtime_skills(
        runtime_skills.to_vec(),
    )));
    providers.extend(explicit_providers.iter().cloned());
    if matches!(policy, SkillDiscoveryPolicy::Defaults) {
        providers.extend(default_skill_providers().into_iter().skip(1));
    }
    providers
}

impl SkillSnapshot {
    /// Render the model-visible skill list served by the `skill://list`
    /// resource. CLI `--list-skills` / TUI `/skills` render the full
    /// (untruncated) text separately in `mink-cli`.
    pub fn format_discoverable_skills(&self) -> String {
        let mut out = String::new();
        for skill in &self.discoverable {
            out.push_str(&format!(
                "- {} [{}]: {}\n",
                crate::capabilities::sanitize_index_text(&skill.skill.name),
                skill.source.model_display_label(),
                crate::capabilities::index_description(&skill.skill.description)
            ));
        }
        out
    }

    pub fn load(
        providers: &[Arc<dyn SkillProvider>],
        ctx: &LoadContext<'_>,
        selected_skills: &[String],
    ) -> Result<Self> {
        let mut order: Vec<usize> = (0..providers.len()).collect();
        order.sort_by(|a, b| {
            providers[*b]
                .priority()
                .cmp(&providers[*a].priority())
                .then_with(|| a.cmp(b))
        });

        let mut all = Vec::new();
        let mut by_name: BTreeMap<String, LoadedSkill> = BTreeMap::new();
        let mut seen_realpaths = BTreeSet::new();
        let mut warnings = Vec::new();

        for idx in order {
            let provider = &providers[idx];
            let mut loaded = provider.load_skills(ctx).map_err(|e| {
                anyhow!(
                    "Error: skill provider {} failed to load skills: {e}",
                    provider.id()
                )
            })?;
            loaded.sort_by(|a, b| a.skill.name.cmp(&b.skill.name));
            for skill in loaded {
                if let Some(path) = &skill.source.source_path
                    && let Ok(realpath) = path.canonicalize()
                    && !seen_realpaths.insert(realpath)
                {
                    continue;
                }
                if let Some(existing) = by_name.get(&skill.skill.name) {
                    warnings.push(CapabilityWarning {
                        provider_id: provider.id().to_string(),
                        message: format!(
                            "skill '{}' from {} is shadowed by {}",
                            skill.skill.name,
                            skill.source.model_display_label(),
                            existing.source.model_display_label()
                        ),
                    });
                    continue;
                }
                if let Some(directory_name) = skill
                    .source
                    .source_path
                    .as_deref()
                    .and_then(|path| path.parent())
                    .and_then(|parent| parent.file_name())
                    .and_then(|name| name.to_str())
                    && directory_name != skill.skill.name
                {
                    warnings.push(CapabilityWarning {
                        provider_id: provider.id().to_string(),
                        message: format!(
                            "skill '{}' adopts frontmatter name '{}' instead of its directory name",
                            directory_name, skill.skill.name
                        ),
                    });
                }
                if matches!(skill.exposure, CapabilityExposure::HostOnly) {
                    warnings.push(CapabilityWarning {
                        provider_id: provider.id().to_string(),
                        message: format!("host-only skill '{}' is hidden", skill.skill.name),
                    });
                }
                by_name.insert(skill.skill.name.clone(), skill.clone());
                all.push(skill);
            }
        }

        let discoverable = all
            .iter()
            .filter(|skill| matches!(skill.exposure, CapabilityExposure::ModelDiscoverable))
            .cloned()
            .collect::<Vec<_>>();
        let mut selected = Vec::new();
        for raw_name in selected_skills {
            let name = raw_name.as_str();
            if !crate::capabilities::source::is_valid_skill_name(name) {
                bail!("Error: invalid selected skill name: {name}");
            }
            let loaded = by_name
                .get(name)
                .ok_or_else(|| anyhow!("Error: skill not found: {name}"))?;
            if matches!(loaded.exposure, CapabilityExposure::HostOnly) {
                bail!("Error: skill is host-only and cannot be selected: {name}");
            }
            selected.push(to_resolved_skill(loaded));
        }

        let dependency_fingerprint =
            compute_dependency_fingerprint(&discoverable, &selected, &by_name);
        Ok(Self {
            all,
            discoverable,
            selected,
            by_name,
            warnings,
            dependency_fingerprint,
        })
    }
}

#[derive(Default)]
pub struct RuntimeSkillProvider {
    skills: Vec<LoadedSkill>,
}

impl RuntimeSkillProvider {
    pub fn from_runtime_skills(skills: Vec<RuntimeSkill>) -> Self {
        Self {
            skills: skills.into_iter().map(runtime_skill_to_loaded).collect(),
        }
    }
}

impl SkillProvider for RuntimeSkillProvider {
    fn id(&self) -> &'static str {
        "runtime-skills"
    }

    fn display_name(&self) -> &'static str {
        "runtime skills"
    }

    fn priority(&self) -> i32 {
        300
    }

    fn load_skills(&self, _ctx: &LoadContext<'_>) -> Result<Vec<LoadedSkill>> {
        for skill in &self.skills {
            if !crate::capabilities::source::is_valid_skill_name(&skill.skill.name) {
                bail!("Error: invalid runtime skill name: {}", skill.skill.name);
            }
        }
        Ok(self.skills.clone())
    }
}

fn runtime_skill_to_loaded(skill: RuntimeSkill) -> LoadedSkill {
    let content = skill.content.replace("${MINK_SKILL_DIR}", "<runtime>");
    let revision = skill
        .revision
        .unwrap_or_else(|| crate::capabilities::fingerprint::sha256_hex(&content));
    LoadedSkill {
        skill: SkillCapability {
            name: skill.name,
            description: skill.description,
            content,
            base_dir: "<runtime>".to_string(),
            disable_model_invocation: matches!(skill.exposure, CapabilityExposure::HostOnly),
        },
        source: SourceMeta {
            provider_id: "runtime-skills".to_string(),
            provider_name: "runtime skills".to_string(),
            level: SourceLevel::Runtime,
            source_path: None,
            display_label: Some("runtime".to_string()),
        },
        exposure: skill.exposure,
        revision,
    }
}

#[derive(Clone, Copy)]
enum SkillBase {
    ProjectClaude,
    ProjectLocal,
    UserClaude,
}

struct FileSystemSkillProvider {
    id: &'static str,
    display_name: &'static str,
    level: SourceLevel,
    priority: i32,
    base: SkillBase,
}

impl FileSystemSkillProvider {
    fn new(
        id: &'static str,
        display_name: &'static str,
        level: SourceLevel,
        priority: i32,
        base: SkillBase,
    ) -> Self {
        Self {
            id,
            display_name,
            level,
            priority,
            base,
        }
    }

    fn base_dir(&self, ctx: &LoadContext<'_>) -> PathBuf {
        match self.base {
            SkillBase::ProjectClaude => ctx.cwd.join(".claude/skills"),
            SkillBase::ProjectLocal => ctx.cwd.join("skills"),
            SkillBase::UserClaude => ctx.home.join(".claude/skills"),
        }
    }
}

impl SkillProvider for FileSystemSkillProvider {
    fn id(&self) -> &'static str {
        self.id
    }

    fn display_name(&self) -> &'static str {
        self.display_name
    }

    fn priority(&self) -> i32 {
        self.priority
    }

    fn load_skills(&self, ctx: &LoadContext<'_>) -> Result<Vec<LoadedSkill>> {
        let base = self.base_dir(ctx);
        let Ok(entries) = fs::read_dir(&base) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let directory_name = entry.file_name().to_string_lossy().to_string();
            if !crate::capabilities::source::is_valid_skill_name(&directory_name) {
                continue;
            }
            let skill_file = entry.path().join("SKILL.md");
            let Ok(raw_content) = fs::read_to_string(&skill_file) else {
                continue;
            };
            let base_dir = skill_file
                .parent()
                .unwrap_or(Path::new(""))
                .display()
                .to_string();
            let content = raw_content.replace("${MINK_SKILL_DIR}", &base_dir);
            let frontmatter = parse_frontmatter(&content);
            // The Agent Skills standard lets a skill name itself in frontmatter;
            // adopt it when valid (the directory name stays the fallback identity).
            let name = frontmatter
                .name
                .as_deref()
                .map(str::trim)
                .filter(|candidate| crate::capabilities::source::is_valid_skill_name(candidate))
                .map(str::to_string)
                .unwrap_or(directory_name);
            let disable_model_invocation = frontmatter.disable_model_invocation;
            let exposure = if frontmatter.hide || disable_model_invocation {
                CapabilityExposure::ModelAddressable
            } else {
                CapabilityExposure::ModelDiscoverable
            };
            let description = resolve_skill_description(&frontmatter, &content);
            out.push(LoadedSkill {
                revision: crate::capabilities::fingerprint::sha256_hex(&content),
                skill: SkillCapability {
                    name,
                    description,
                    content,
                    base_dir,
                    disable_model_invocation,
                },
                source: SourceMeta {
                    provider_id: self.id.to_string(),
                    provider_name: self.display_name.to_string(),
                    level: self.level.clone(),
                    source_path: Some(skill_file),
                    display_label: Some(crate::capabilities::source::display_label(
                        &base, ctx.cwd, ctx.home,
                    )),
                },
                exposure,
            });
        }
        Ok(out)
    }
}

struct BuiltInSkillProvider;

impl SkillProvider for BuiltInSkillProvider {
    fn id(&self) -> &'static str {
        "built-in-skills"
    }

    fn display_name(&self) -> &'static str {
        "built-in skills"
    }

    fn priority(&self) -> i32 {
        0
    }

    fn load_skills(&self, _ctx: &LoadContext<'_>) -> Result<Vec<LoadedSkill>> {
        Ok(crate::assets::embedded_skills::all()
            .into_iter()
            .map(|skill| {
                let content = skill.content.replace("${MINK_SKILL_DIR}", "<built-in>");
                LoadedSkill {
                    revision: crate::capabilities::fingerprint::sha256_hex(&content),
                    skill: SkillCapability {
                        name: skill.name.to_string(),
                        description: skill.description.to_string(),
                        content,
                        base_dir: "<built-in>".to_string(),
                        disable_model_invocation: false,
                    },
                    source: SourceMeta {
                        provider_id: self.id().to_string(),
                        provider_name: self.display_name().to_string(),
                        level: SourceLevel::BuiltIn,
                        source_path: None,
                        display_label: Some("built-in".to_string()),
                    },
                    exposure: CapabilityExposure::ModelDiscoverable,
                }
            })
            .collect())
    }
}

#[derive(Default)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    when_to_use: Option<String>,
    hide: bool,
    disable_model_invocation: bool,
}

/// Parse the YAML-subset frontmatter written at the top of SKILL.md files.
///
/// Supported subset (what the Agent Skills ecosystem actually writes):
/// - single-line `key: value` (surrounding quotes trimmed);
/// - block scalars `key: >` / `key: |` with optional chomping (`-`/`+`) and
///   indentation indicators, consumed while following lines stay indented;
/// - `#` comment lines.
///
/// Unknown keys are ignored. A block without a closing `---` is treated as
/// absent so a malformed header never leaks body lines into parsed fields.
fn parse_frontmatter(content: &str) -> Frontmatter {
    let mut out = Frontmatter::default();
    let mut lines = content.lines().peekable();
    if lines.next().map(str::trim) != Some("---") {
        return out;
    }
    loop {
        let Some(raw) = lines.next() else {
            // Unclosed frontmatter: treat the header as absent.
            return Frontmatter::default();
        };
        let line = raw.trim_end_matches('\r');
        let trimmed = line.trim();
        if trimmed == "---" {
            return out;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if let Some(style) = block_scalar_style(value) {
            let body = collect_block_scalar(&mut lines);
            let text = render_block_scalar(&body, style);
            if !text.is_empty() {
                assign_frontmatter_field(&mut out, key, &text);
            }
            continue;
        }
        let value = trim_yaml_scalar(value);
        if value.is_empty() {
            continue;
        }
        assign_frontmatter_field(&mut out, key, value);
    }
}

/// Chomping/indentation indicators are accepted for syntax compatibility with
/// real SKILL.md files; the parsed value is trimmed either way.
struct BlockStyle {
    literal: bool,
}

fn block_scalar_style(raw: &str) -> Option<BlockStyle> {
    let first = raw.chars().next()?;
    let literal = match first {
        '>' => false,
        '|' => true,
        _ => return None,
    };
    let header = raw.split('#').next().unwrap_or(raw).trim();
    for ch in header.chars().skip(1) {
        match ch {
            '-' | '+' | '0'..='9' => {}
            _ => return None,
        }
    }
    Some(BlockStyle { literal })
}

/// Consume the indented body of a block scalar; blank lines inside the block
/// are kept as paragraph breaks, trailing blanks are not part of the value.
fn collect_block_scalar<'a, I>(lines: &mut std::iter::Peekable<I>) -> Vec<String>
where
    I: Iterator<Item = &'a str>,
{
    let mut body: Vec<String> = Vec::new();
    let mut pending_blanks = 0usize;
    while let Some(peek) = lines.peek() {
        let line = peek.trim_end_matches('\r');
        if line.trim().is_empty() {
            pending_blanks += 1;
            lines.next();
            continue;
        }
        if !line.starts_with(' ') && !line.starts_with('\t') {
            break;
        }
        for _ in 0..pending_blanks {
            body.push(String::new());
        }
        pending_blanks = 0;
        let consumed = lines.next().expect("peeked line must exist");
        body.push(consumed.trim_end_matches('\r').to_string());
    }
    body
}

/// Strip the common indentation, join the body, and trim the result. `>`
/// folds single newlines into spaces (blank lines become paragraph breaks);
/// `|` keeps the line structure.
fn render_block_scalar(lines: &[String], style: BlockStyle) -> String {
    let indent = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start_matches([' ', '\t']).len())
        .min()
        .unwrap_or(0);
    let stripped = lines
        .iter()
        .map(|line| {
            if line.len() > indent {
                &line[indent..]
            } else {
                ""
            }
        })
        .collect::<Vec<_>>();
    let text = if style.literal {
        stripped.join("\n")
    } else {
        let mut out = String::new();
        let mut paragraph: Vec<&str> = Vec::new();
        for line in &stripped {
            if line.trim().is_empty() {
                if !paragraph.is_empty() {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&paragraph.join(" "));
                    paragraph.clear();
                }
            } else {
                paragraph.push(line.trim_end());
            }
        }
        if !paragraph.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&paragraph.join(" "));
        }
        out
    };
    text.trim().to_string()
}

fn assign_frontmatter_field(out: &mut Frontmatter, key: &str, value: &str) {
    match key {
        "name" => out.name = Some(value.to_string()),
        "description" => out.description = Some(value.to_string()),
        "when_to_use" => out.when_to_use = Some(value.to_string()),
        "hide" => out.hide = parse_bool(value),
        "disable-model-invocation" => out.disable_model_invocation = parse_bool(value),
        _ => {}
    }
}

/// `description` (from frontmatter or the first content line) with the
/// optional `when_to_use` trigger text appended, matching how the Agent
/// Skills listing combines both fields.
fn resolve_skill_description(frontmatter: &Frontmatter, content: &str) -> String {
    let base = frontmatter
        .description
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| extract_skill_summary(content));
    match frontmatter
        .when_to_use
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(when) => format!("{} — {when}", base.trim()),
        None => base,
    }
}

pub(crate) fn extract_skill_summary(content: &str) -> String {
    if let Some(description) = parse_frontmatter(content)
        .description
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return description;
    }
    body_after_frontmatter(content)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && *line != "---")
        .unwrap_or("")
        .chars()
        .take(120)
        .collect()
}

/// Content after the closing `---` of a frontmatter block; the full content
/// when there is no header or the header is unclosed.
fn body_after_frontmatter(content: &str) -> &str {
    let Some(idx) = content.find('\n') else {
        return content;
    };
    let (first, after) = content.split_at(idx + 1);
    if first.trim() != "---" {
        return content;
    }
    let mut rest = after;
    loop {
        match rest.find('\n') {
            Some(idx) => {
                let (line, after) = rest.split_at(idx + 1);
                if line.trim() == "---" {
                    return after;
                }
                rest = after;
            }
            None => {
                if rest.trim() == "---" {
                    return "";
                }
                return content;
            }
        }
    }
}

fn parse_bool(value: &str) -> bool {
    matches!(
        trim_yaml_scalar(value).to_ascii_lowercase().as_str(),
        "true" | "yes" | "1"
    )
}

fn trim_yaml_scalar(value: &str) -> &str {
    value.trim().trim_matches('"').trim_matches('\'')
}

fn to_resolved_skill(loaded: &LoadedSkill) -> ResolvedSkill {
    ResolvedSkill {
        info: SkillInfo {
            name: loaded.skill.name.clone(),
            description: loaded.skill.description.clone(),
            base_dir: loaded.skill.base_dir.clone(),
        },
        content: loaded.skill.content.clone(),
    }
}

fn compute_dependency_fingerprint(
    discoverable: &[LoadedSkill],
    selected: &[ResolvedSkill],
    by_name: &BTreeMap<String, LoadedSkill>,
) -> String {
    let mut input = String::new();
    for skill in discoverable {
        input.push_str("discoverable\0");
        input.push_str(&skill.skill.name);
        input.push('\0');
        input.push_str(&skill.skill.description);
        input.push('\0');
        input.push_str(crate::capabilities::source::exposure_label(&skill.exposure));
        input.push('\0');
        input.push_str(&skill.source.provider_id);
        input.push('\0');
        input.push_str(&skill.revision);
        input.push('\0');
    }
    for skill in selected {
        input.push_str("selected\0");
        input.push_str(&skill.info.name);
        input.push('\0');
        if let Some(loaded) = by_name.get(&skill.info.name) {
            input.push_str(&loaded.revision);
        }
        input.push('\0');
        input.push_str(&crate::capabilities::fingerprint::sha256_hex(
            &skill.content,
        ));
        input.push('\0');
    }
    crate::capabilities::fingerprint::sha256_hex(&input)
}

#[cfg(test)]
#[path = "skills_tests.rs"]
mod tests;
