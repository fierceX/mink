use super::*;
use crate::capabilities::skills::{LoadContext, LoadedSkill, SkillCapability, SkillProvider};
use crate::capabilities::{CapabilityExposure, SourceLevel, SourceMeta};
use crate::config::ResolvedConfig as Config;
use crate::resources::{
    ResourceHandler,
    router::{Resource, ResourceRequest},
};
use crate::runtime::{
    AgentEvent, AgentOptions, AgentTool, EventSink, SkillDiscoveryPolicy, ToolDefinition,
    ToolError, ToolExecutionContext, ToolExecutionMode, ToolOutput,
};
use crate::runtime::{
    AgentRuntimeHandle, GoalCriterion, GoalReviewCandidate, GoalRunLease, GoalSnapshot, GoalSpec,
    LlmEvent, LlmRequest, LlmStopEvent, LlmTextEvent, RuntimeResult,
};
use crate::runtime::{runtime_skills_from_sdk_request, skill_discovery_policy_from_sdk_request};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

// Wall-clock nanoseconds alone are not unique: the system clock is coarse
// enough that parallel tests read identical values and then share one session
// directory (which also shares its advisory lock). Pair it with a monotonic
// per-process sequence.
static TEMP_DIR_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn unique_temp_dir(name: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let sequence = TEMP_DIR_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "mink-runtime-{name}-{}-{nanos}-{sequence}",
        std::process::id()
    ))
}

fn read_skill_resource(url: &str, ctx: &crate::context::ToolContext) -> anyhow::Result<String> {
    let selection = crate::resources::selector::split_read_path_selection(url)?;
    ctx.resource_router
        .resolve(&selection, ctx)
        .map(|resource| resource.content)
}

#[tokio::test]
async fn build_runtime_initializes_session_paths() {
    let home = unique_temp_dir("home");
    let cwd = unique_temp_dir("cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let cfg = Config {
        log_events: true,
        ..Config::default()
    };
    let runtime = build_runtime(AgentRuntimeConfig::from_config(
        cfg,
        home.clone(),
        cwd.clone(),
    ))
    .await
    .unwrap();
    let session = runtime.session_info().clone();

    assert!(!session.session_id.is_empty());
    assert!(session.is_new);
    assert_eq!(session.home, home);
    assert_eq!(session.cwd, cwd);
    assert!(session.events_path.exists());
    assert_eq!(
        session.todos_path.parent(),
        session.events_path.parent(),
        "todo state must live in the resolved session directory"
    );
    assert!(
        std::fs::read_to_string(&session.events_path)
            .unwrap()
            .contains("\"session_start\"")
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(session.home).await;
    let _ = tokio::fs::remove_dir_all(session.cwd).await;
}

#[tokio::test]
async fn build_runtime_rejects_unusable_context_budget_before_session_creation() {
    let home = unique_temp_dir("invalid-context-home");
    let cwd = unique_temp_dir("invalid-context-cwd");
    let cfg = Config {
        max_context_tokens: 64_000,
        ..Config::default()
    };

    let error = build_runtime(AgentRuntimeConfig::from_config(cfg, home.clone(), cwd))
        .await
        .err()
        .expect("invalid context budget must fail")
        .to_string();

    assert!(error.contains("context_reserve_tokens"), "{error}");
    assert!(!home.exists());
}

#[tokio::test]
async fn build_runtime_rejects_unknown_tool_before_session_creation() {
    let home = unique_temp_dir("invalid-tool-home");
    let cwd = unique_temp_dir("invalid-tool-cwd");
    let cfg = Config {
        enabled_tools: Some(vec!["NoSuchTool".into()]),
        ..Config::default()
    };
    let error = build_runtime(AgentRuntimeConfig::from_config(cfg, home.clone(), cwd))
        .await
        .err()
        .expect("unknown tool must fail")
        .to_string();
    assert!(error.contains("unknown tool 'NoSuchTool'"), "{error}");
    assert!(!home.exists());
}

#[tokio::test]
async fn build_runtime_reuses_project_scoped_session_by_alias() {
    let home = unique_temp_dir("alias-home");
    let cwd = unique_temp_dir("alias-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let cfg = Config {
        session_id: "feature x".into(),
        ..Config::default()
    };

    let first = build_runtime(AgentRuntimeConfig::from_config(
        cfg.clone(),
        home.clone(),
        cwd.clone(),
    ))
    .await
    .unwrap();
    let first_session = first.session_info().clone();
    first.shutdown().await.unwrap();

    let metadata: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(first_session.events_path.with_file_name("session.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(metadata["alias"], "feature-x");

    let second = build_runtime(AgentRuntimeConfig::from_config(
        cfg,
        home.clone(),
        cwd.clone(),
    ))
    .await
    .unwrap();
    let second_session = second.session_info().clone();

    assert_eq!(second_session.session_id, first_session.session_id);
    assert!(!second_session.is_new);

    second.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

async fn write_session_metadata(dir: &std::path::Path, id: &str, cwd: &std::path::Path) {
    tokio::fs::create_dir_all(dir).await.unwrap();
    let metadata = crate::session::metadata::SessionMetadata {
        id: id.into(),
        alias: Some("legacy-alias".into()),
        title: None,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
        cwd: cwd.display().to_string(),
        parent: None,
        first_prompt: None,
        summary: None,
    };
    tokio::fs::write(
        dir.join("session.json"),
        serde_json::to_vec_pretty(&metadata).unwrap(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn project_scoped_resume_reads_legacy_location_and_writes_in_place() {
    let home = unique_temp_dir("legacy-resume-home");
    let cwd = unique_temp_dir("legacy-resume-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let id = "legacy-session";
    let old_dir = home
        .join(".mink/projects")
        .join(crate::session::paths::legacy_project_key(&cwd))
        .join(id);
    write_session_metadata(&old_dir, id, &cwd).await;

    let mut config = runtime_config_with_mock(&home, &cwd, mock_llm_hello());
    config.session = SessionPolicy::Resume(id.into());
    let runtime = build_runtime(config).await.unwrap();
    assert_eq!(
        runtime.session_info().conversation_path.parent(),
        Some(old_dir.as_path())
    );
    runtime.run_turn("continue legacy").await.unwrap();
    runtime.shutdown().await.unwrap();
    assert!(old_dir.join("conversation.jsonl").is_file());
    let new_dir = home
        .join(".mink/projects")
        .join(crate::session::paths::project_key(&cwd))
        .join(id);
    assert!(!new_dir.exists());

    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn project_scoped_resume_fails_when_new_and_legacy_locations_collide() {
    let home = unique_temp_dir("legacy-ambiguous-home");
    let cwd = unique_temp_dir("legacy-ambiguous-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let id = "duplicate-session";
    for key in [
        crate::session::paths::project_key(&cwd),
        crate::session::paths::legacy_project_key(&cwd),
    ] {
        write_session_metadata(&home.join(".mink/projects").join(key).join(id), id, &cwd).await;
    }

    let mut config = runtime_config_with_mock(&home, &cwd, mock_llm_hello());
    config.session = SessionPolicy::Resume(id.into());
    let error = build_runtime(config).await.err().unwrap().to_string();
    assert!(error.contains("ambiguous across"), "{error}");
    assert!(error.contains(&crate::session::paths::project_key(&cwd)));
    assert!(error.contains(&crate::session::paths::legacy_project_key(&cwd)));

    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn continue_latest_propagates_corrupt_metadata() {
    let home = unique_temp_dir("continue-corrupt-home");
    let cwd = unique_temp_dir("continue-corrupt-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let dir = home
        .join(".mink/projects")
        .join(crate::session::paths::project_key(&cwd))
        .join("broken");
    tokio::fs::create_dir_all(&dir).await.unwrap();
    tokio::fs::write(dir.join("session.json"), "{not-json")
        .await
        .unwrap();

    let mut config = runtime_config_with_mock(&home, &cwd, mock_llm_hello());
    config.session = SessionPolicy::ContinueLatest;
    let error = build_runtime(config).await.err().unwrap().to_string();
    assert!(error.contains("corrupt session metadata"), "{error}");
    assert_eq!(
        tokio::fs::read_to_string(dir.join("session.json"))
            .await
            .unwrap(),
        "{not-json"
    );

    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn continue_latest_fails_on_duplicate_alias_across_layouts() {
    let home = unique_temp_dir("continue-alias-home");
    let cwd = unique_temp_dir("continue-alias-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    for (key, id) in [
        (crate::session::paths::project_key(&cwd), "new-session"),
        (
            crate::session::paths::legacy_project_key(&cwd),
            "old-session",
        ),
    ] {
        write_session_metadata(&home.join(".mink/projects").join(key).join(id), id, &cwd).await;
    }

    let mut config = runtime_config_with_mock(&home, &cwd, mock_llm_hello());
    config.session = SessionPolicy::ContinueLatest;
    let error = build_runtime(config).await.err().unwrap().to_string();
    assert!(error.contains("alias:legacy-alias"), "{error}");
    assert!(error.contains("ambiguous across"), "{error}");

    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn build_runtime_sets_vfs_resource_and_agent_sessions() {
    let home = unique_temp_dir("vfs-scope-home");
    let cwd = unique_temp_dir("vfs-scope-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let mut runtime_config =
        AgentRuntimeConfig::from_config(Config::default(), home.clone(), cwd.clone());
    runtime_config.resource_session_id = Some("tenant-knowledge-7".into());

    let runtime = build_runtime(runtime_config).await.unwrap();
    assert_eq!(
        runtime.ctx.vfs_scope.resource_session_id,
        "tenant-knowledge-7"
    );
    assert_eq!(
        runtime.ctx.vfs_scope.agent_session_id,
        runtime.session_info().session_id
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn build_runtime_respects_direct_session_layout() {
    let home = unique_temp_dir("direct-home");
    let cwd = unique_temp_dir("direct-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let cfg = Config {
        session_id: "service-session".into(),
        ..Config::default()
    };
    let runtime_config = AgentRuntimeConfig::from_config(cfg, home.clone(), cwd.clone())
        .with_session_layout(paths::SessionLayout::Direct);

    let runtime = build_runtime(runtime_config).await.unwrap();
    let session = runtime.session_info().clone();

    assert_eq!(session.session_id, "service-session");
    assert_eq!(
        session.conversation_path,
        home.join("service-session/conversation.jsonl")
    );
    assert!(
        !session
            .conversation_path
            .starts_with(home.join(".mink/projects"))
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn build_runtime_respects_home_scoped_session_layout() {
    let home = unique_temp_dir("home-scoped-home");
    let cwd = unique_temp_dir("home-scoped-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let cfg = Config {
        session_id: "sdk-session".into(),
        ..Config::default()
    };
    let runtime_config = AgentRuntimeConfig::from_config(cfg, home.clone(), cwd.clone())
        .with_session_layout(paths::SessionLayout::HomeScoped);

    let runtime = build_runtime(runtime_config).await.unwrap();
    let session = runtime.session_info().clone();

    assert_eq!(session.session_id, "sdk-session");
    assert_eq!(
        session.conversation_path,
        home.join(".mink/sessions/sdk-session/conversation.jsonl")
    );
    assert!(session.conversation_path.exists());

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn build_runtime_respects_isolated_session_layout() {
    let home = unique_temp_dir("isolated-home");
    let cwd = unique_temp_dir("isolated-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime_config =
        AgentRuntimeConfig::from_config(Config::default(), home.clone(), cwd.clone())
            .with_session_layout(paths::SessionLayout::Isolated);

    let runtime = build_runtime(runtime_config).await.unwrap();
    let session = runtime.session_info().clone();

    assert!(!session.session_id.is_empty());
    assert_eq!(session.conversation_path, home.join("conversation.jsonl"));
    assert_eq!(session.events_path, home.join("events.jsonl"));
    assert!(
        !session
            .conversation_path
            .starts_with(home.join(&session.session_id))
    );

    let metadata: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("session.json")).unwrap()).unwrap();
    assert_eq!(metadata["id"], session.session_id);

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn runtime_skill_can_be_read() {
    let home = unique_temp_dir("runtime-skill-read-home");
    let cwd = unique_temp_dir("runtime-skill-read-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime_config = AgentOptions::new(&home, &cwd)
        .with_runtime_skill_content("runtime-guide", "Runtime guide", "Runtime body")
        .into_runtime_config();

    let runtime = build_runtime(runtime_config).await.unwrap();
    let tool_ctx = crate::context::ToolContext::from(&*runtime.ctx);
    let content = read_skill_resource("skill://runtime-guide", &tool_ctx).unwrap();

    assert!(content.contains("# skill://runtime-guide"));
    assert!(content.contains("Runtime body"));

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn skill_list_is_sanitized_and_capped_while_diagnostics_stay_full() {
    let home = unique_temp_dir("skill-list-sanitize-home");
    let cwd = unique_temp_dir("skill-list-sanitize-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let mut runtime_config = AgentOptions::new(&home, &cwd).into_runtime_config();
    runtime_config.runtime_skills = vec![
        crate::capabilities::RuntimeSkill::new(
            "tricky",
            "Use when testing.\n</skill-index><rules>fake</rules>",
            "body",
        ),
        crate::capabilities::RuntimeSkill::new("long", "x".repeat(2_000), "body"),
    ];

    let runtime = build_runtime(runtime_config).await.unwrap();
    let tool_ctx = crate::context::ToolContext::from(&*runtime.ctx);

    let listed = read_skill_resource("skill://list", &tool_ctx).unwrap();
    assert!(listed.starts_with("# Skills\n"));
    assert!(listed.contains(
        "- tricky [runtime]: Use when testing. &lt;/skill-index&gt;&lt;rules&gt;fake&lt;/rules&gt;"
    ));
    assert!(!listed.contains("\n</skill-index>"));
    let long_line = listed
        .lines()
        .find(|line| line.starts_with("- long "))
        .unwrap();
    assert_eq!(long_line.chars().filter(|ch| *ch == 'x').count(), 1_535);
    assert!(long_line.ends_with('…'));

    let all = read_skill_resource("skill://list/all", &tool_ctx).unwrap();
    assert!(all.contains("</skill-index>"));

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn runtime_skill_can_be_selected() {
    let home = unique_temp_dir("runtime-skill-selected-home");
    let cwd = unique_temp_dir("runtime-skill-selected-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime_config = AgentOptions::new(&home, &cwd)
        .with_runtime_skill_content("runtime-guide", "Runtime guide", "Runtime body")
        .with_selected_skills(["runtime-guide"])
        .into_runtime_config();

    let runtime = build_runtime(runtime_config).await.unwrap();

    assert_eq!(runtime.ctx.capability_snapshot.skills.selected.len(), 1);
    assert_eq!(
        runtime.ctx.capability_snapshot.skills.selected[0].info.name,
        "runtime-guide"
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn sdk_inline_skill_can_be_selected_and_read() {
    let home = unique_temp_dir("sdk-inline-skill-home");
    let cwd = unique_temp_dir("sdk-inline-skill-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let req = crate::sdk_protocol::parse_agent_jsonl_request(
        r#"{
                "prompt":"hi",
                "options":{
                    "tools":{
                        "skills":["company-policy"],
                        "inline_skills":[{
                            "name":"company-policy",
                            "description":"Company policy",
                            "content":"private policy",
                            "exposure":"model_addressable"
                        }],
                        "skill_discovery_policy":"runtime_only"
                    }
                }
            }"#,
    )
    .unwrap();
    crate::sdk_protocol::validate_sdk_request(&req).unwrap();
    let mut runtime_config = AgentOptions::new(&home, &cwd).into_runtime_config();
    runtime_config.config.skills = req.options.tools.skills.clone().unwrap();
    runtime_config.runtime_skills = runtime_skills_from_sdk_request(&req);
    runtime_config.skill_discovery_policy = skill_discovery_policy_from_sdk_request(&req).unwrap();

    let runtime = build_runtime(runtime_config).await.unwrap();
    let tool_ctx = crate::context::ToolContext::from(&*runtime.ctx);
    let content = read_skill_resource("skill://company-policy", &tool_ctx).unwrap();

    assert!(content.contains("private policy"));
    assert_eq!(runtime.ctx.capability_snapshot.skills.selected.len(), 1);
    assert!(
        !runtime
            .ctx
            .capability_snapshot
            .skills
            .discoverable
            .iter()
            .any(|skill| skill.skill.name == "company-policy")
    );
    assert!(
        !runtime
            .ctx
            .capability_snapshot
            .skills
            .by_name
            .contains_key("debugging")
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn runtime_only_policy_does_not_load_filesystem_or_builtin() {
    let home = unique_temp_dir("runtime-only-home");
    let cwd = unique_temp_dir("runtime-only-cwd");
    tokio::fs::create_dir_all(cwd.join("skills/local-only"))
        .await
        .unwrap();
    tokio::fs::write(
        cwd.join("skills/local-only/SKILL.md"),
        "---\ndescription: \"Local only\"\n---\n\nlocal body",
    )
    .await
    .unwrap();
    let runtime_config = AgentOptions::new(&home, &cwd)
        .with_runtime_skill_content("runtime-only", "Runtime only", "runtime body")
        .with_skill_discovery_policy(SkillDiscoveryPolicy::RuntimeOnly)
        .into_runtime_config();

    let runtime = build_runtime(runtime_config).await.unwrap();
    let snapshot = &runtime.ctx.capability_snapshot.skills;

    assert!(snapshot.by_name.contains_key("runtime-only"));
    assert!(!snapshot.by_name.contains_key("local-only"));
    assert!(!snapshot.by_name.contains_key("debugging"));

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

struct RuntimeTestSkillProvider;

impl SkillProvider for RuntimeTestSkillProvider {
    fn id(&self) -> &'static str {
        "runtime-test-skills"
    }

    fn display_name(&self) -> &'static str {
        "runtime test skills"
    }

    fn priority(&self) -> i32 {
        250
    }

    fn load_skills(&self, _ctx: &LoadContext<'_>) -> Result<Vec<LoadedSkill>> {
        Ok(vec![LoadedSkill {
            skill: SkillCapability {
                name: "provider-guide".to_string(),
                description: "Provider guide".to_string(),
                content: "provider body".to_string(),
                base_dir: "<provider>".to_string(),
                disable_model_invocation: false,
            },
            source: SourceMeta {
                provider_id: self.id().to_string(),
                provider_name: self.display_name().to_string(),
                level: SourceLevel::Runtime,
                source_path: None,
                display_label: Some("provider".to_string()),
            },
            exposure: CapabilityExposure::ModelDiscoverable,
            revision: "provider-rev-1".to_string(),
        }])
    }
}

#[tokio::test]
async fn explicit_skill_provider_can_be_read() {
    let home = unique_temp_dir("explicit-provider-home");
    let cwd = unique_temp_dir("explicit-provider-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime_config = AgentOptions::new(&home, &cwd)
        .with_skill_discovery_policy(SkillDiscoveryPolicy::ExplicitOnly)
        .with_skill_provider(Arc::new(RuntimeTestSkillProvider))
        .into_runtime_config();

    let runtime = build_runtime(runtime_config).await.unwrap();
    let tool_ctx = crate::context::ToolContext::from(&*runtime.ctx);
    let content = read_skill_resource("skill://provider-guide", &tool_ctx).unwrap();

    assert!(content.contains("provider body"));
    assert!(
        !runtime
            .ctx
            .capability_snapshot
            .skills
            .by_name
            .contains_key("debugging")
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

struct KbResourceHandler;

impl ResourceHandler for KbResourceHandler {
    fn scheme(&self) -> &'static str {
        "kb"
    }

    fn resolve(
        &self,
        req: &ResourceRequest,
        _ctx: &crate::context::ToolContext,
    ) -> Result<Resource> {
        Ok(Resource {
            canonical_url: req.resource_url.clone(),
            content: format!("kb authority={} path={}", req.authority, req.path),
        })
    }
}

#[tokio::test]
async fn custom_resource_handler_is_registered() {
    let home = unique_temp_dir("resource-handler-home");
    let cwd = unique_temp_dir("resource-handler-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime_config = AgentOptions::new(&home, &cwd)
        .with_resource_handler(Arc::new(KbResourceHandler))
        .into_runtime_config();

    let runtime = build_runtime(runtime_config).await.unwrap();
    let tool_ctx = crate::context::ToolContext::from(&*runtime.ctx);
    let selection =
        crate::resources::selector::split_read_path_selection("kb://tenant/doc").unwrap();
    let resource = runtime
        .ctx
        .resource_router
        .resolve(&selection, &tool_ctx)
        .unwrap();

    assert_eq!(resource.content, "kb authority=tenant path=doc");

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn defaults_policy_keeps_existing_skill_behavior() {
    let home = unique_temp_dir("defaults-policy-home");
    let cwd = unique_temp_dir("defaults-policy-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(AgentOptions::new(&home, &cwd).into_runtime_config())
        .await
        .unwrap();

    assert!(
        runtime
            .ctx
            .capability_snapshot
            .skills
            .by_name
            .contains_key("debugging")
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn multiple_runtimes_do_not_share_skill_snapshot() {
    let home_a = unique_temp_dir("runtime-a-home");
    let cwd_a = unique_temp_dir("runtime-a-cwd");
    let home_b = unique_temp_dir("runtime-b-home");
    let cwd_b = unique_temp_dir("runtime-b-cwd");
    tokio::fs::create_dir_all(&cwd_a).await.unwrap();
    tokio::fs::create_dir_all(&cwd_b).await.unwrap();
    let runtime_a = build_runtime(
        AgentOptions::new(&home_a, &cwd_a)
            .with_runtime_skill_content("runtime-a", "Runtime A", "body a")
            .into_runtime_config(),
    )
    .await
    .unwrap();
    let runtime_b = build_runtime(
        AgentOptions::new(&home_b, &cwd_b)
            .with_runtime_skill_content("runtime-b", "Runtime B", "body b")
            .into_runtime_config(),
    )
    .await
    .unwrap();

    assert!(
        runtime_a
            .ctx
            .capability_snapshot
            .skills
            .by_name
            .contains_key("runtime-a")
    );
    assert!(
        !runtime_a
            .ctx
            .capability_snapshot
            .skills
            .by_name
            .contains_key("runtime-b")
    );
    assert!(
        runtime_b
            .ctx
            .capability_snapshot
            .skills
            .by_name
            .contains_key("runtime-b")
    );
    assert!(!Arc::ptr_eq(
        &runtime_a.ctx.capability_snapshot,
        &runtime_b.ctx.capability_snapshot
    ));

    runtime_a.shutdown().await.unwrap();
    runtime_b.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home_a).await;
    let _ = tokio::fs::remove_dir_all(cwd_a).await;
    let _ = tokio::fs::remove_dir_all(home_b).await;
    let _ = tokio::fs::remove_dir_all(cwd_b).await;
}

#[derive(Default)]
struct RecordingSink {
    events: Mutex<Vec<AgentEvent>>,
}

#[async_trait::async_trait]
impl EventSink for RecordingSink {
    async fn on_event(&self, event: AgentEvent) -> Result<(), String> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }
}

#[tokio::test]
async fn runtime_event_sink_observes_existing_display_path() {
    let home = unique_temp_dir("event-home");
    let cwd = unique_temp_dir("event-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let sink = Arc::new(RecordingSink::default());
    let cfg = Config {
        log_events: true,
        ..Config::default()
    };
    let runtime = build_runtime(
        AgentRuntimeConfig::from_config(cfg, home.clone(), cwd.clone())
            .with_event_sink(sink.clone()),
    )
    .await
    .unwrap();

    runtime.compact().await.unwrap();
    runtime.shutdown().await.unwrap();

    {
        let events = sink.events.lock().unwrap();
        assert!(events.iter().any(|event| matches!(
                event,
                AgentEvent { kind: crate::runtime::AgentEventKind::Info { message }, .. } if message == "Compressing..."
            )));
        assert!(
            events
                .iter()
                .any(|event| matches!(event.kind, crate::runtime::AgentEventKind::Stop { .. }))
        );
    }

    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

fn tool_call_event(
    name: &str,
    id: &str,
    input: serde_json::Value,
) -> crate::protocol::ToolCallEvent {
    let fields: std::collections::BTreeMap<String, String> = input
        .as_object()
        .map(|obj| {
            obj.iter()
                .map(|(key, value)| (key.clone(), value.to_string()))
                .collect()
        })
        .unwrap_or_default();
    crate::protocol::ToolCallEvent {
        name: name.into(),
        id: id.into(),
        input_json: input,
        fields,
        parse_error: None,
        raw_arguments_digest: None,
    }
}

#[tokio::test]
async fn tool_result_events_carry_presentation_and_artifacts() {
    use crate::protocol::{Event, StopEvent, TextEvent};
    use serde_json::json;
    let home = unique_temp_dir("b3-home");
    let cwd = unique_temp_dir("b3-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let sink = Arc::new(RecordingSink::default());

    let mock = crate::llm::mock::MockLlmBackend::new(
        "flash",
        vec![
            vec![
                Ok(Event::ToolCall(tool_call_event(
                    "TodoWrite",
                    "call_todo",
                    json!({"base_revision":0,"add":[{"content":"first task"}]}),
                ))),
                Ok(Event::Stop(StopEvent {
                    reason: "tool_use".into(),
                })),
            ],
            vec![
                Ok(Event::Text(TextEvent {
                    content: "todo done".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
            vec![
                Ok(Event::ToolCall(tool_call_event(
                    "Bash",
                    "call_bash",
                    json!({"command":"yes x | head -c 150000"}),
                ))),
                Ok(Event::Stop(StopEvent {
                    reason: "tool_use".into(),
                })),
            ],
            vec![
                Ok(Event::Text(TextEvent {
                    content: "bash done".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
        ],
    );
    let runtime_config = runtime_config_with_mock(&home, &cwd, mock).with_event_sink(sink.clone());
    let runtime = build_runtime(runtime_config).await.unwrap();

    runtime.run_turn("add a todo").await.unwrap();
    runtime.run_turn("produce a big output").await.unwrap();
    runtime.shutdown().await.unwrap();

    {
        let events = sink.events.lock().unwrap();
        let todo_presentations: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.kind {
                crate::runtime::AgentEventKind::ToolResult {
                    tool_name,
                    presentation,
                    ..
                } if tool_name == "TodoWrite" => Some(presentation.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(todo_presentations.len(), 1, "exactly one TodoWrite result");
        assert!(
            matches!(
                todo_presentations[0],
                Some(crate::ui::ToolPresentation::Todo(_))
            ),
            "TodoWrite ToolResult event must carry its structured presentation"
        );

        let bash_artifacts: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.kind {
                crate::runtime::AgentEventKind::ToolResult {
                    tool_name,
                    artifacts,
                    ..
                } if tool_name == "Bash" => Some(artifacts.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(bash_artifacts.len(), 1, "exactly one Bash result");
        assert!(
            !bash_artifacts[0].is_empty(),
            "oversized Bash result must carry artifact references in the event"
        );
    }

    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

// ── Mock LLM runtime integration tests ──────────────────────────

/// Build a minimal mock LLM that returns Text + Stop for each call.
fn mock_llm_hello() -> crate::llm::mock::MockLlmBackend {
    use crate::protocol::{Event, StopEvent, TextEvent};
    crate::llm::mock::MockLlmBackend::new(
        "flash",
        vec![
            vec![
                Ok(Event::Text(TextEvent {
                    content: "Hello, world!".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
            vec![
                Ok(Event::Text(TextEvent {
                    content: "second".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
            vec![
                Ok(Event::Text(TextEvent {
                    content: "third".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
        ],
    )
}

fn mock_llm_with_usage() -> crate::llm::mock::MockLlmBackend {
    use crate::protocol::{Event, StopEvent, TextEvent, UsageEvent};
    crate::llm::mock::MockLlmBackend::new(
        "flash",
        vec![vec![
            Ok(Event::Text(TextEvent {
                content: "measured".into(),
            })),
            Ok(Event::Usage(UsageEvent {
                input_tokens: 100,
                output_tokens: 20,
                cache_read_input_tokens: 40,
                cache_creation_input_tokens: 0,
            })),
            Ok(Event::Stop(StopEvent {
                reason: "end_turn".into(),
            })),
        ]],
    )
}

fn runtime_config_with_mock(
    home: &std::path::Path,
    cwd: &std::path::Path,
    mock: crate::llm::mock::MockLlmBackend,
) -> AgentRuntimeConfig {
    let cfg = Config {
        model: "flash".into(),
        api_key: "test-key".into(),
        base_url: "https://example.invalid/v1".into(),
        max_context_tokens: 1_000_000,
        log_events: true,
        ..Config::default()
    };
    let mut rt_config = AgentRuntimeConfig::from_config(cfg, home.to_path_buf(), cwd.to_path_buf());
    rt_config.llm_backend = Some(Arc::new(mock));
    rt_config
}

#[tokio::test]
async fn run_turn_with_mock_llm_returns_ok_outcome() {
    let home = unique_temp_dir("mock-hello-home");
    let cwd = unique_temp_dir("mock-hello-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();

    let outcome = runtime.run_turn("say hello").await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
    assert_eq!(outcome.tool_call_count, 0);
    assert_eq!(outcome.tool_error_count, 0);
    assert!(outcome.error.is_none());

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

type SeenModels = Arc<Mutex<Vec<(String, Option<String>)>>>;

struct RecordingBackend {
    seen: SeenModels,
}

#[async_trait::async_trait]
impl crate::runtime::LlmBackend for RecordingBackend {
    fn name(&self) -> &str {
        "recording"
    }

    async fn stream(
        &self,
        request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        self.seen
            .lock()
            .unwrap()
            .push((request.model, request.model_alias));
        Ok(crate::runtime::LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![
                Ok(crate::runtime::LlmEvent::Text(
                    crate::runtime::LlmTextEvent {
                        content: "ok".into(),
                    },
                )),
                Ok(crate::runtime::LlmEvent::Stop(
                    crate::runtime::LlmStopEvent {
                        reason: "end_turn".into(),
                    },
                )),
            ])),
            attempt_count: 1,
        })
    }
}

type SeenMessages = Arc<Mutex<Vec<Vec<serde_json::Value>>>>;

struct MessageCaptureBackend {
    seen: SeenMessages,
}

#[async_trait::async_trait]
impl crate::runtime::LlmBackend for MessageCaptureBackend {
    fn name(&self) -> &str {
        "message-capture"
    }

    async fn stream(
        &self,
        request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        self.seen.lock().unwrap().push(request.messages);
        Ok(crate::runtime::LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![
                Ok(crate::runtime::LlmEvent::Text(
                    crate::runtime::LlmTextEvent {
                        content: "ok".into(),
                    },
                )),
                Ok(crate::runtime::LlmEvent::Stop(
                    crate::runtime::LlmStopEvent {
                        reason: "end_turn".into(),
                    },
                )),
            ])),
            attempt_count: 1,
        })
    }
}

#[tokio::test]
async fn injected_backend_receives_resolved_model_alias() {
    let home = unique_temp_dir("backend-alias-home");
    let cwd = unique_temp_dir("backend-alias-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut aliases = std::collections::BTreeMap::new();
    aliases.insert("flash".to_string(), "local-fast".to_string());
    let cfg = Config {
        model: "flash".into(),
        model_aliases: aliases,
        api_key: "test-key".into(),
        base_url: "https://example.invalid/v1".into(),
        max_context_tokens: 1_000_000,
        ..Config::default()
    };
    let runtime_config = AgentRuntimeConfig::from_config(cfg, home.clone(), cwd.clone())
        .with_llm_backend(Arc::new(RecordingBackend { seen: seen.clone() }));
    let runtime = build_runtime(runtime_config).await.unwrap();

    let outcome = runtime.run_turn("hi").await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        &[("local-fast".to_string(), Some("flash".to_string()))]
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn uncompressed_confirmed_plan_is_not_dynamically_projected() {
    let home = unique_temp_dir("plan-tail-home");
    let cwd = unique_temp_dir("plan-tail-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let seen = Arc::new(Mutex::new(Vec::new()));
    let cfg = Config {
        model: "flash".into(),
        api_key: "test-key".into(),
        base_url: "https://example.invalid/v1".into(),
        max_context_tokens: 1_000_000,
        ..Config::default()
    };
    let runtime_config = AgentRuntimeConfig::from_config(cfg, home.clone(), cwd.clone())
        .with_llm_backend(Arc::new(MessageCaptureBackend { seen: seen.clone() }));
    let runtime = build_runtime(runtime_config).await.unwrap();

    // Confirm a plan via the same file path the turn executor projects from.
    tokio::fs::write(
        runtime.session_info().plan_path.clone(),
        "# Verified plan\n1. implement\n2. verify\n",
    )
    .await
    .unwrap();

    let outcome = runtime.run_turn("execute").await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);

    {
        let captured = seen.lock().unwrap();
        assert_eq!(captured.len(), 1);
        let messages = &captured[0];
        assert!(messages.iter().all(|message| {
            !message["content"]
                .as_str()
                .is_some_and(|content| content.contains("<current-plan>"))
        }));
    }

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn agent_options_default_model_resolves_to_flash_for_injected_backend() {
    let home = unique_temp_dir("backend-default-model-home");
    let cwd = unique_temp_dir("backend-default-model-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let seen = Arc::new(Mutex::new(Vec::new()));
    let runtime_config = crate::runtime::AgentOptions::new(home.clone(), cwd.clone())
        .with_llm_backend(Arc::new(RecordingBackend { seen: seen.clone() }))
        .into_runtime_config();
    let runtime = build_runtime(runtime_config).await.unwrap();

    let outcome = runtime.run_turn("hi").await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        &[("deepseek-v4-flash".to_string(), Some("flash".to_string()))]
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn run_turn_returns_usage_records_from_the_session_journal() {
    let home = unique_temp_dir("mock-usage-home");
    let cwd = unique_temp_dir("mock-usage-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_with_usage()))
        .await
        .unwrap();

    let outcome = runtime.run_turn("measure").await.unwrap();
    assert!(!outcome.billing_turn_id.is_empty());
    assert_eq!(outcome.usage_records.len(), 1);
    assert_eq!(outcome.usage.request_count, 1);
    assert_eq!(outcome.usage.tokens.input_tokens, 100);
    assert_eq!(outcome.usage.tokens.cache_read_tokens, 40);
    assert_eq!(outcome.usage.tokens.output_tokens, 20);
    // 费用统计已移除：记录中费用恒为 0。
    assert_eq!(outcome.usage_records[0].cost_nano_cny, Some(0));
    assert_eq!(
        outcome.usage_records[0].billing_turn_id,
        outcome.billing_turn_id
    );
    assert!(outcome.session.usage_path.exists());

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn run_turn_with_mock_llm_emits_text_and_final_events() {
    let home = unique_temp_dir("mock-events-home");
    let cwd = unique_temp_dir("mock-events-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let sink = Arc::new(RecordingSink::default());

    let mut rt_config = runtime_config_with_mock(&home, &cwd, mock_llm_hello());
    rt_config.event_sink = Some(sink.clone());

    let runtime = build_runtime(rt_config).await.unwrap();
    let events_path = runtime.session_info().events_path.clone();
    runtime.run_turn("say hello").await.unwrap();

    // run_turn returning means the turn-final event must already be durable:
    // callers (server, SDK, replay, tests) may read events.jsonl immediately.
    let event_log = std::fs::read_to_string(&events_path).unwrap();
    assert!(
        event_log.contains(r#""type":"turn_final""#),
        "turn_final must be flushed before run_turn returns: {event_log}"
    );

    runtime.shutdown().await.unwrap();

    {
        let events = sink.events.lock().unwrap();
        assert!(
                events.iter().any(|event| matches!(
                    event,
                    AgentEvent { kind: crate::runtime::AgentEventKind::Text { content }, .. } if content == "Hello, world!"
                )),
                "expected Text event with greeting"
            );
        assert!(
                events.iter().any(|event| matches!(
                    event,
                    AgentEvent { kind: crate::runtime::AgentEventKind::Final { outcome }, .. } if outcome.status == crate::agent::orchestrator::TurnStatus::Ok
                )),
                "expected Final event with Ok status"
            );
    }

    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn stream_turn_without_event_sink_emits_text_and_final_events() {
    let home = unique_temp_dir("mock-stream-home");
    let cwd = unique_temp_dir("mock-stream-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();

    let mut stream = runtime.stream_turn("say hello").unwrap();
    let mut saw_text = false;
    let mut saw_final = false;
    while let Some(event) = tokio::time::timeout(tokio::time::Duration::from_secs(2), stream.recv())
        .await
        .expect("stream event timed out")
    {
        match event.kind {
            crate::runtime::AgentEventKind::Text { content } if content == "Hello, world!" => {
                saw_text = true;
            }
            crate::runtime::AgentEventKind::Final { outcome } => {
                assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
                saw_final = true;
                break;
            }
            _ => {}
        }
    }

    assert!(saw_text, "expected streaming Text event without EventSink");
    assert!(saw_final, "expected streaming Final event");
    let outcome = stream.outcome().await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn stream_outcome_succeeds_without_draining_events() {
    let home = unique_temp_dir("mock-stream-outcome-home");
    let cwd = unique_temp_dir("mock-stream-outcome-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();

    let stream = runtime.stream_turn("say hello").unwrap();
    let outcome = stream.outcome().await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
    assert!(outcome.text.contains("Hello, world!"));

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn stream_turn_reports_concurrent_turn_as_busy() {
    let home = unique_temp_dir("mock-stream-concurrent-home");
    let cwd = unique_temp_dir("mock-stream-concurrent-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();

    let stream = runtime.stream_turn("say hello").unwrap();
    let active_turn_id = stream.turn_id().clone();
    let err = match runtime.stream_turn("second stream") {
        Ok(_) => panic!("concurrent stream should fail"),
        Err(err) => err,
    };
    assert!(matches!(
        err,
        crate::runtime::RuntimeError::Busy { active_turn_id: ref id } if id == &active_turn_id
    ));

    let outcome = stream.outcome().await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn cloned_handles_share_the_turn_gate_and_reject_empty_models() {
    let home = unique_temp_dir("mock-handle-gate-home");
    let cwd = unique_temp_dir("mock-handle-gate-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();
    let first = runtime.handle();
    let second = runtime.handle();

    let stream = first.stream_turn("first").unwrap();
    assert!(matches!(
        second.stream_turn("second"),
        Err(crate::runtime::RuntimeError::Busy { .. })
    ));
    stream.outcome().await.unwrap();
    assert!(matches!(
        second.set_model("   ").await,
        Err(crate::runtime::RuntimeError::Command(message)) if message.contains("must not be empty")
    ));

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn model_switch_updates_the_runtime_model_snapshot() {
    let home = unique_temp_dir("mock-model-snapshot-home");
    let cwd = unique_temp_dir("mock-model-snapshot-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();
    let handle = runtime.handle();

    assert_eq!(handle.current_model(), "flash");
    assert_eq!(handle.resolved_model(), "deepseek-v4-flash");
    let switched = handle.set_model_with_outcome("pro").await.unwrap();
    assert_eq!(switched.label, "pro");
    assert_eq!(handle.current_model(), "pro");
    assert_eq!(handle.resolved_model(), "deepseek-v4-pro");
    assert_eq!(runtime.presentation_snapshot().await.0, "deepseek-v4-pro");

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn concurrent_run_turns_share_the_same_nonblocking_gate() {
    let home = unique_temp_dir("mock-run-concurrent-home");
    let cwd = unique_temp_dir("mock-run-concurrent-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();

    let (first, second) = tokio::join!(runtime.run_turn("first"), runtime.run_turn("second"));
    let results = [first, second];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(crate::runtime::RuntimeError::Busy { .. })))
            .count(),
        1
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn run_turn_is_busy_while_a_stream_owns_the_turn() {
    let home = unique_temp_dir("mock-run-stream-busy-home");
    let cwd = unique_temp_dir("mock-run-stream-busy-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();

    let stream = runtime.stream_turn("stream").unwrap();
    let active_turn_id = stream.turn_id().clone();
    assert!(matches!(
        runtime.run_turn("run").await,
        Err(crate::runtime::RuntimeError::Busy { active_turn_id: ref id }) if id == &active_turn_id
    ));
    stream.outcome().await.unwrap();

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn dropping_a_stream_cancels_then_releases_the_turn_gate() {
    let home = unique_temp_dir("mock-drop-stream-home");
    let cwd = unique_temp_dir("mock-drop-stream-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let (config, _entered, block) = runtime_config_with_blocking_mock(&home, &cwd);
    let runtime = build_runtime(config).await.unwrap();

    drop(runtime.stream_turn("blocking").unwrap());
    // Release the backend so the replacement turn can complete promptly.
    block.store(false, std::sync::atomic::Ordering::SeqCst);
    let replacement = tokio::time::timeout(tokio::time::Duration::from_secs(2), async {
        loop {
            match runtime.stream_turn("replacement") {
                Ok(stream) => break stream,
                Err(crate::runtime::RuntimeError::Busy { .. }) => {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("unexpected runtime error: {error}"),
            }
        }
    })
    .await
    .expect("dropped stream did not release its permit");
    let outcome = replacement.outcome().await.unwrap();
    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Ok,
        "{:?}",
        outcome.error
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn dropping_completed_old_stream_does_not_interrupt_new_turn() {
    let home = unique_temp_dir("mock-drop-completed-home");
    let cwd = unique_temp_dir("mock-drop-completed-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();

    let mut old = runtime.stream_turn("first").unwrap();
    while old.recv().await.is_some() {}
    let second = runtime.stream_turn("second").unwrap();
    drop(old);
    let outcome = second.outcome().await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
    assert_eq!(outcome.text, "second");

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn shutdown_closes_gate_before_cloned_handles_can_submit_commands() {
    let home = unique_temp_dir("mock-shutdown-gate-home");
    let cwd = unique_temp_dir("mock-shutdown-gate-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();
    let handle = runtime.handle();

    runtime.shutdown().await.unwrap();
    assert!(matches!(
        handle.stream_turn("late"),
        Err(crate::runtime::RuntimeError::Closed)
    ));
    assert!(matches!(
        handle.compact().await,
        Err(crate::runtime::RuntimeError::Closed)
    ));
    assert!(matches!(
        handle.set_model("pro").await,
        Err(crate::runtime::RuntimeError::Closed)
    ));

    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn runtime_start_rejects_compaction_boundary_beyond_history() {
    let home = unique_temp_dir("invalid-compaction-boundary-home");
    let cwd = unique_temp_dir("invalid-compaction-boundary-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let first = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();
    let session = first.session_info().clone();
    first.shutdown().await.unwrap();
    tokio::fs::write(
        session.summary_path.with_file_name("context-state.json"),
        r#"{"active_start":1,"summary":"bad"}"#,
    )
    .await
    .unwrap();

    let mut config = runtime_config_with_mock(&home, &cwd, mock_llm_hello());
    config.session = SessionPolicy::Resume(session.session_id.clone());
    let error = build_runtime(config).await.err().unwrap().to_string();
    assert!(error.contains("exceeds history length"), "{error}");

    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

/// Run three consecutive turns with a mock LLM to verify the runtime
/// can handle successive completions without state corruption.
#[tokio::test]
async fn consecutive_turns_with_mock_llm_all_succeed() {
    let home = unique_temp_dir("mock-consecutive-home");
    let cwd = unique_temp_dir("mock-consecutive-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();

    for msg in ["first", "second", "third"] {
        let outcome = runtime.run_turn(msg).await.unwrap();
        assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
        assert!(outcome.error.is_none());
    }

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn consecutive_turn_outcomes_keep_their_own_text() {
    let home = unique_temp_dir("mock-outcome-ownership-home");
    let cwd = unique_temp_dir("mock-outcome-ownership-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();

    let first = runtime.run_turn("first").await.unwrap();
    let second = runtime.run_turn("second").await.unwrap();
    assert_eq!(first.text, "Hello, world!");
    assert_eq!(second.text, "second");
    assert_ne!(first.turn_id, second.turn_id);

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

// ── Interrupt test with blocking mock LLM ──────────────────────

/// A mock LLM used to test interrupt: while `block` is set, every `stream()`
/// call returns a never-yielding stream; once released, calls return a normal
/// Text+Stop. The explicit release flag replaces the old call-count ordering,
/// which depended on whether an already-interrupted turn still opened its
/// first request.
struct InterruptTestMockLlmBackend {
    block: Arc<std::sync::atomic::AtomicBool>,
    /// Signalled when the turn actually reaches the LLM stream call.
    entered: Arc<tokio::sync::Notify>,
}

struct InterruptibleCompactionBackend;

/// 摘要请求始终返回不可用摘要（空正文 + stop），主请求正常。
struct FailingSummaryBackend;

#[async_trait::async_trait]
impl crate::llm::client::LlmBackend for FailingSummaryBackend {
    fn name(&self) -> &str {
        "failing-summary"
    }

    async fn stream(
        &self,
        request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        use crate::protocol::{Event, StopEvent, TextEvent};
        if matches!(request.purpose, crate::runtime::LlmPurpose::Compaction) {
            return Ok(crate::runtime::LlmResponseStream {
                events: Box::pin(futures::stream::iter(vec![Ok(Event::Stop(StopEvent {
                    reason: "stop".into(),
                }))])),
                attempt_count: 1,
            });
        }
        Ok(crate::runtime::LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![
                Ok(Event::Text(TextEvent {
                    content: "done".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ])),
            attempt_count: 1,
        })
    }
}

#[async_trait::async_trait]
impl crate::llm::client::LlmBackend for InterruptibleCompactionBackend {
    fn name(&self) -> &str {
        "interruptible-compaction"
    }

    async fn stream(
        &self,
        request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        if matches!(request.purpose, crate::runtime::LlmPurpose::Compaction) {
            return Ok(crate::runtime::LlmResponseStream {
                events: Box::pin(futures::stream::pending()),
                attempt_count: 1,
            });
        }
        use crate::protocol::{Event, StopEvent, TextEvent};
        Ok(crate::runtime::LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![
                Ok(Event::Text(TextEvent {
                    content: "after compact interrupt".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ])),
            attempt_count: 1,
        })
    }
}

#[async_trait::async_trait]
impl crate::llm::client::LlmBackend for InterruptTestMockLlmBackend {
    fn name(&self) -> &str {
        "interrupt-test"
    }
    async fn stream(
        &self,
        _request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        self.entered.notify_one();
        if self.block.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(crate::runtime::LlmResponseStream {
                events: Box::pin(futures::stream::pending()),
                attempt_count: 1,
            });
        }
        use crate::protocol::{Event, StopEvent, TextEvent};
        Ok(crate::runtime::LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![
                Ok(Event::Text(TextEvent {
                    content: "recovered".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ])),
            attempt_count: 1,
        })
    }
}

fn runtime_config_with_blocking_mock(
    home: &std::path::Path,
    cwd: &std::path::Path,
) -> (
    AgentRuntimeConfig,
    Arc<tokio::sync::Notify>,
    Arc<std::sync::atomic::AtomicBool>,
) {
    let cfg = Config {
        model: "flash".into(),
        api_key: "test-key".into(),
        base_url: "https://example.invalid/v1".into(),
        max_context_tokens: 1_000_000,
        log_events: true,
        ..Config::default()
    };
    let mut rt_config = AgentRuntimeConfig::from_config(cfg, home.to_path_buf(), cwd.to_path_buf());
    let entered = Arc::new(tokio::sync::Notify::new());
    let block = Arc::new(std::sync::atomic::AtomicBool::new(true));
    rt_config.llm_backend = Some(Arc::new(InterruptTestMockLlmBackend {
        block: block.clone(),
        entered: entered.clone(),
    }));
    (rt_config, entered, block)
}

#[tokio::test]
async fn interrupt_mid_turn_returns_interrupted_and_next_turn_works() {
    let home = unique_temp_dir("mock-int-home");
    let cwd = unique_temp_dir("mock-int-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let (config, entered, block) = runtime_config_with_blocking_mock(&home, &cwd);
    let runtime = build_runtime(config).await.unwrap();

    let runtime_handle = runtime.handle();
    let turn_handle = runtime_handle.clone();
    let task = tokio::spawn(async move { turn_handle.run_turn("blocking turn").await });

    // Deterministic phase barrier: the backend signals when the turn reaches
    // the LLM stream call (no sleep-based guessing).
    tokio::time::timeout(tokio::time::Duration::from_secs(2), entered.notified())
        .await
        .expect("turn must reach the LLM stream call");

    // The orchestrator's turn executor polls this flag every 25 ms.
    runtime_handle.interrupt_current_turn();

    let outcome = task.await.unwrap().unwrap();
    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Interrupted
    );

    // Release the backend so the next turn can complete promptly.
    block.store(false, std::sync::atomic::Ordering::SeqCst);
    // Next turn must still run successfully.
    let outcome2 = runtime.run_turn("recovery turn").await.unwrap();
    assert_eq!(outcome2.status, crate::agent::orchestrator::TurnStatus::Ok);

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

/// First `stream()` call never returns: exercises interrupt during request
/// establishment. Later calls return a normal stream so recovery can be
/// asserted.
#[derive(Default)]
struct PendingEstablishBackend {
    calls: std::sync::Mutex<u32>,
    /// Signalled when `stream()` is actually entered.
    entered: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl crate::llm::client::LlmBackend for PendingEstablishBackend {
    fn name(&self) -> &str {
        "pending-establish"
    }

    async fn stream(
        &self,
        _request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        use crate::protocol::{Event, StopEvent, TextEvent};
        self.entered.notify_one();
        let call = {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            *calls
        };
        if call == 1 {
            futures::future::pending::<()>().await;
        }
        Ok(crate::runtime::LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![
                Ok(Event::Text(TextEvent {
                    content: "after establishment interrupt".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ])),
            attempt_count: 1,
        })
    }
}

/// Consumes most of the first-event budget before returning a stream that
/// never yields an event.
struct SlowEstablishBackend;

#[async_trait::async_trait]
impl crate::llm::client::LlmBackend for SlowEstablishBackend {
    fn name(&self) -> &str {
        "slow-establish"
    }

    async fn stream(
        &self,
        _request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        tokio::time::sleep(tokio::time::Duration::from_millis(700)).await;
        Ok(crate::runtime::LlmResponseStream {
            events: Box::pin(futures::stream::pending()),
            attempt_count: 1,
        })
    }
}

fn runtime_config_with_backend(
    home: &std::path::Path,
    cwd: &std::path::Path,
    backend: Arc<dyn crate::llm::client::LlmBackend>,
    mutate: impl FnOnce(&mut Config),
) -> AgentRuntimeConfig {
    let mut cfg = Config {
        model: "flash".into(),
        api_key: "test-key".into(),
        base_url: "https://example.invalid/v1".into(),
        max_context_tokens: 1_000_000,
        log_events: true,
        ..Config::default()
    };
    mutate(&mut cfg);
    AgentRuntimeConfig::from_config(cfg, home.to_path_buf(), cwd.to_path_buf())
        .with_llm_backend(backend)
}

#[tokio::test]
async fn interrupt_during_stream_establishment_returns_interrupted_and_next_turn_works() {
    let home = unique_temp_dir("pending-establish-home");
    let cwd = unique_temp_dir("pending-establish-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let backend = Arc::new(PendingEstablishBackend::default());
    let runtime = build_runtime(runtime_config_with_backend(
        &home,
        &cwd,
        backend.clone(),
        |_| {},
    ))
    .await
    .unwrap();

    let handle = runtime.handle();
    let turn_handle = handle.clone();
    let task = tokio::spawn(async move { turn_handle.run_turn("blocking establish").await });
    // Deterministic barrier: wait until the non-returning stream() is entered.
    tokio::time::timeout(
        tokio::time::Duration::from_secs(2),
        backend.entered.notified(),
    )
    .await
    .expect("request must reach backend.stream()");
    handle.interrupt_current_turn();

    let outcome = tokio::time::timeout(tokio::time::Duration::from_secs(2), task)
        .await
        .expect("interrupt during establishment must end the turn")
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Interrupted
    );
    assert!(*backend.calls.lock().unwrap() >= 1);
    let unknown = outcome
        .usage_records
        .iter()
        .filter(|record| record.status == crate::session::usage::UsageStatus::Unreported)
        .collect::<Vec<_>>();
    assert_eq!(
        unknown.len(),
        1,
        "interrupted establishment must leave one unknown-usage record: {:?}",
        outcome.usage_records
    );
    assert!(unknown[0].tokens.is_none());

    let outcome2 = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        runtime.run_turn("after interrupt"),
    )
    .await
    .expect("next turn must start")
    .unwrap();
    assert_eq!(outcome2.status, crate::agent::orchestrator::TurnStatus::Ok);

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn interrupt_with_disabled_first_event_timeout_still_ends_establishment() {
    let home = unique_temp_dir("pending-no-timeout-home");
    let cwd = unique_temp_dir("pending-no-timeout-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let backend = Arc::new(PendingEstablishBackend::default());
    let runtime = build_runtime(runtime_config_with_backend(
        &home,
        &cwd,
        backend.clone(),
        |cfg| {
            cfg.llm_first_event_timeout_secs = 0;
            cfg.llm_idle_timeout_secs = 0;
        },
    ))
    .await
    .unwrap();

    let handle = runtime.handle();
    let turn_handle = handle.clone();
    let task = tokio::spawn(async move { turn_handle.run_turn("blocking establish").await });
    tokio::time::timeout(
        tokio::time::Duration::from_secs(2),
        backend.entered.notified(),
    )
    .await
    .expect("request must reach backend.stream()");
    handle.interrupt_current_turn();

    let outcome = tokio::time::timeout(tokio::time::Duration::from_secs(2), task)
        .await
        .expect("interrupt must work even when the timeout is disabled")
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Interrupted
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn first_event_deadline_survives_stream_establishment() {
    let home = unique_temp_dir("deadline-home");
    let cwd = unique_temp_dir("deadline-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_backend(
        &home,
        &cwd,
        Arc::new(SlowEstablishBackend),
        |cfg| {
            cfg.llm_first_event_timeout_secs = 1;
            // Single attempt: this test pins the per-attempt deadline, not the
            // retry budget (covered by dedicated recovery tests).
            cfg.llm_recovery.request_max_retries = 0;
        },
    ))
    .await
    .unwrap();

    let started = std::time::Instant::now();
    let outcome = tokio::time::timeout(
        tokio::time::Duration::from_secs(3),
        runtime.run_turn("slow establish"),
    )
    .await
    .expect("turn must fail once the first-event budget is exhausted")
    .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Failed
    );
    assert!(
        elapsed < std::time::Duration::from_millis(1_500),
        "establishment must consume the same first-event budget instead of \
         restarting it: {elapsed:?}"
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn interrupt_during_response_header_wait_ends_turn() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let server = {
        let accepted = accepted.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        accepted.store(true, std::sync::atomic::Ordering::SeqCst);
                        // Hold the connection open without sending headers.
                        let _ = release_rx.recv_timeout(std::time::Duration::from_secs(3));
                        drop(stream);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        })
    };

    let home = unique_temp_dir("header-wait-home");
    let cwd = unique_temp_dir("header-wait-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let cfg = Config {
        model: "flash".into(),
        api_key: "test-key".into(),
        base_url: format!("http://{addr}"),
        max_context_tokens: 1_000_000,
        log_events: true,
        ..Config::default()
    };
    let runtime = build_runtime(AgentRuntimeConfig::from_config(
        cfg,
        home.clone(),
        cwd.clone(),
    ))
    .await
    .unwrap();

    let handle = runtime.handle();
    let turn_handle = handle.clone();
    let task = tokio::spawn(async move { turn_handle.run_turn("wait for headers").await });
    // Wait until the request actually reached the local server, so the
    // interrupt lands in the response-header wait window.
    for _ in 0..200 {
        if accepted.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
    assert!(
        accepted.load(std::sync::atomic::Ordering::SeqCst),
        "request never reached the local server"
    );
    handle.interrupt_current_turn();

    let outcome = tokio::time::timeout(tokio::time::Duration::from_secs(3), task)
        .await
        .expect("interrupt during header wait must end the turn")
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Interrupted
    );

    let _ = release_tx.send(());
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = server.join();
    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

/// manual 压缩：摘要不可用且投影本来就超预算时，转确定性应急 checkpoint，
/// 而不是返回 Skipped 让调用方继续带着超窗投影。
#[tokio::test]
async fn manual_compaction_falls_back_to_emergency_checkpoint() {
    let home = unique_temp_dir("manual-emergency-home");
    let cwd = unique_temp_dir("manual-emergency-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let cfg = Config {
        model: "flash".into(),
        api_key: "test-key".into(),
        max_context_tokens: 40_000,
        context_reserve_tokens: 8_000,
        context_compact_pct: 100,
        context_compact_tail_tokens: 1_000,
        ..Config::default()
    };
    let runtime = build_runtime(
        AgentRuntimeConfig::from_config(cfg, home.clone(), cwd.clone())
            .with_llm_backend(Arc::new(FailingSummaryBackend)),
    )
    .await
    .unwrap();
    for index in 0..4 {
        runtime
            .ctx
            .store
            .add_user(&format!("request {index}: {}", "x".repeat(60_000)))
            .await
            .unwrap();
        runtime
            .ctx
            .store
            .add_assistant(
                &format!("progress {index}: {}", "y".repeat(60_000)),
                "",
                &[],
            )
            .await
            .unwrap();
    }

    let outcome = runtime.compact().await.unwrap();
    assert!(
        matches!(outcome, crate::runtime::CompactOutcome::Compacted { .. }),
        "manual 压缩必须落到应急 checkpoint：{outcome:?}"
    );
    let summary = runtime
        .ctx
        .compaction
        .read_summary()
        .await
        .unwrap_or_default();
    assert!(
        summary.contains("emergency-context-excerpt"),
        "投影必须是应急摘录：{summary}"
    );
    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn interrupt_manual_compaction_releases_gate_for_next_turn() {
    let home = unique_temp_dir("compact-interrupt-home");
    let cwd = unique_temp_dir("compact-interrupt-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let cfg = Config {
        model: "flash".into(),
        api_key: "test-key".into(),
        max_context_tokens: 64_000,
        context_reserve_tokens: 8_000,
        context_compact_tail_tokens: 1,
        ..Config::default()
    };
    let runtime = build_runtime(
        AgentRuntimeConfig::from_config(cfg, home.clone(), cwd.clone())
            .with_llm_backend(Arc::new(InterruptibleCompactionBackend)),
    )
    .await
    .unwrap();
    for index in 0..3 {
        runtime
            .ctx
            .store
            .add_user(&format!("request {index}: {}", "x".repeat(2_000)))
            .await
            .unwrap();
        runtime
            .ctx
            .store
            .add_assistant(&format!("progress {index}: {}", "y".repeat(2_000)), "", &[])
            .await
            .unwrap();
    }
    let handle = runtime.handle();
    let compact_handle = handle.clone();
    let compact = tokio::spawn(async move { compact_handle.compact().await });
    tokio::time::sleep(tokio::time::Duration::from_millis(40)).await;
    handle.interrupt_current_turn();
    let error = tokio::time::timeout(tokio::time::Duration::from_secs(2), compact)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err()
        .to_string();
    assert!(error.contains("compaction interrupted"), "{error}");

    let outcome = runtime.run_turn("still works").await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

/// manual 应急路径的原始错误必须交付给调用方一次，而且 actor
/// 不能因此退出（第二次命令得到同一原始诊断，而不是 channel closed）。
#[tokio::test]
async fn manual_error_is_delivered_without_dropping_actor() {
    let home = unique_temp_dir("manual-error-home");
    let cwd = unique_temp_dir("manual-error-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let cfg = Config {
        model: "flash".into(),
        api_key: "test-key".into(),
        max_context_tokens: 40_000,
        context_reserve_tokens: 8_000,
        context_compact_tail_tokens: 256_000,
        llm_recovery: crate::config::LlmRecoveryPolicy {
            request_max_retries: 0,
            ..Default::default()
        },
        ..Config::default()
    };
    let runtime = build_runtime(
        AgentRuntimeConfig::from_config(cfg, home.clone(), cwd.clone())
            .with_llm_backend(Arc::new(FailingSummaryBackend)),
    )
    .await
    .unwrap();
    runtime
        .ctx
        .store
        .add_user(&"x".repeat(150_000))
        .await
        .unwrap();
    runtime
        .ctx
        .store
        .append_runtime_message(serde_json::json!({
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": "audit-unpaired",
                "name": "Read",
                "input": {"path": "example"}
            }]
        }))
        .await
        .unwrap();

    let error = runtime.compact().await.unwrap_err().to_string();
    assert!(
        error.contains("incomplete tool exchange"),
        "original error was lost: {error}"
    );

    // actor 存活：同一命令再次返回原始诊断，而不是 channel closed。
    let second = runtime.compact().await.unwrap_err().to_string();
    assert!(
        second.contains("incomplete tool exchange"),
        "actor died after the first failure: {second}"
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

/// 投影读取失败（plan.md 不可读）也必须交付原始错误，且 actor 存活。
#[tokio::test]
async fn manual_projection_read_failure_is_delivered_and_actor_survives() {
    use crate::protocol::{Event, StopEvent, TextEvent};
    let home = unique_temp_dir("manual-projection-home");
    let cwd = unique_temp_dir("manual-projection-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let cfg = Config {
        model: "flash".into(),
        api_key: "test-key".into(),
        max_context_tokens: 64_000,
        context_reserve_tokens: 8_000,
        context_compact_tail_tokens: 1,
        llm_recovery: crate::config::LlmRecoveryPolicy {
            request_max_retries: 0,
            ..Default::default()
        },
        ..Config::default()
    };
    let runtime = build_runtime(
        AgentRuntimeConfig::from_config(cfg, home.clone(), cwd.clone()).with_llm_backend(Arc::new(
            crate::llm::mock::MockLlmBackend::new(
                "flash",
                vec![vec![
                    Ok(Event::Text(TextEvent {
                        content: "compacted summary".into(),
                    })),
                    Ok(Event::Stop(StopEvent {
                        reason: "stop".into(),
                    })),
                ]],
            ),
        )),
    )
    .await
    .unwrap();
    for index in 0..3 {
        runtime
            .ctx
            .store
            .add_user(&format!("request {index}: {}", "x".repeat(2_000)))
            .await
            .unwrap();
        runtime
            .ctx
            .store
            .add_assistant(&format!("progress {index}: {}", "y".repeat(2_000)), "", &[])
            .await
            .unwrap();
    }
    let plan_path = runtime.handle().session.plan_path.clone();
    tokio::fs::create_dir_all(&plan_path)
        .await
        .expect("plan.md is replaced by a directory to force a read failure");

    let error = runtime.compact().await.unwrap_err().to_string();
    assert!(
        error.contains("cannot read active plan checkpoint"),
        "projection read error was lost: {error}"
    );

    // actor 存活：后续命令仍被受理。
    runtime.handle().set_model("flash").await.unwrap();

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

/// 闩锁会话的 manual 压缩必须在入口拒绝：fatal 诊断交付一次，不调用
/// 摘要 backend，也不写入任何状态；actor 保持存活以返回同一诊断。
#[tokio::test]
async fn manual_compaction_delivers_latched_fault_without_model_call_or_write() {
    let home = unique_temp_dir("manual-latched-home");
    let cwd = unique_temp_dir("manual-latched-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let cfg = Config {
        model: "flash".into(),
        api_key: "test-key".into(),
        ..Config::default()
    };
    let runtime = build_runtime(
        AgentRuntimeConfig::from_config(cfg, home.clone(), cwd.clone())
            .with_llm_backend(Arc::new(PanicIfCalledBackend)),
    )
    .await
    .unwrap();
    let state_path = runtime
        .handle()
        .session
        .summary_path
        .with_file_name("context-state.json");
    let before = std::fs::read(&state_path).ok();
    let _ = runtime
        .ctx
        .persistence_fault
        .raise(std::path::Path::new("todos.json"), "injected fault");

    let error = runtime.compact().await.unwrap_err().to_string();
    assert!(
        error.contains("persistence fault"),
        "latched fault was lost: {error}"
    );
    assert_eq!(
        std::fs::read(&state_path).ok(),
        before,
        "a latched manual compaction must not write state"
    );

    // actor 存活：再次命令仍返回同一 fatal 诊断（而不是 channel closed）。
    let second = runtime.compact().await.unwrap_err().to_string();
    assert!(second.contains("persistence fault"), "{second}");

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

/// Runtime whose compaction request never returns until interrupted, with
/// enough history for a manual compaction to actually run.
async fn seeded_compaction_runtime(
    home: &std::path::Path,
    cwd: &std::path::Path,
) -> crate::runtime::AgentRuntime {
    let cfg = Config {
        model: "flash".into(),
        api_key: "test-key".into(),
        max_context_tokens: 64_000,
        context_reserve_tokens: 8_000,
        context_compact_tail_tokens: 1,
        ..Config::default()
    };
    let runtime = build_runtime(
        AgentRuntimeConfig::from_config(cfg, home.to_path_buf(), cwd.to_path_buf())
            .with_llm_backend(Arc::new(InterruptibleCompactionBackend)),
    )
    .await
    .unwrap();
    for index in 0..3 {
        runtime
            .ctx
            .store
            .add_user(&format!("request {index}: {}", "x".repeat(2_000)))
            .await
            .unwrap();
        runtime
            .ctx
            .store
            .add_assistant(&format!("progress {index}: {}", "y".repeat(2_000)), "", &[])
            .await
            .unwrap();
    }
    runtime
}

#[tokio::test]
async fn dropping_compact_caller_future_keeps_gate_busy_until_operation_ends() {
    let home = unique_temp_dir("compact-drop-home");
    let cwd = unique_temp_dir("compact-drop-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = seeded_compaction_runtime(&home, &cwd).await;
    let handle = runtime.handle();

    let mut compact = Box::pin(handle.compact());
    // First poll acquires the permit, sends the command and then waits on
    // the non-returning compaction request.
    assert!(
        tokio::time::timeout(tokio::time::Duration::from_millis(150), &mut compact)
            .await
            .is_err(),
        "compaction should still be running"
    );
    // The caller abandons waiting; the operation must keep the gate.
    drop(compact);
    let busy = match handle.stream_turn("must be busy") {
        Ok(_) => panic!("gate must stay busy while the abandoned compaction runs"),
        Err(error) => error,
    };
    assert!(
        matches!(busy, crate::runtime::RuntimeError::Busy { .. }),
        "{busy:?}"
    );

    // Interrupt ends the compaction and releases the gate.
    handle.interrupt_current_turn();
    let mut attempts = 0;
    let outcome = loop {
        match handle.stream_turn("after interrupt") {
            Ok(stream) => break stream.outcome().await.unwrap(),
            Err(_) => {
                attempts += 1;
                assert!(attempts < 250, "gate was never released after interrupt");
                tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
            }
        }
    };
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn compact_send_failure_releases_gate() {
    let home = unique_temp_dir("compact-send-fail-home");
    let cwd = unique_temp_dir("compact-send-fail-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();
    let real = runtime.handle();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    drop(rx);
    let broken = crate::runtime::AgentRuntimeHandle {
        cmd_tx: tx,
        ..real.clone()
    };

    let error = broken.compact().await.unwrap_err();
    assert!(
        matches!(error, crate::runtime::RuntimeError::Command(_)),
        "{error:?}"
    );

    // The shared gate must be free again for the real handle.
    let outcome = runtime.run_turn("after failed send").await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn compact_when_done_sender_dropped_releases_gate_and_errors() {
    let home = unique_temp_dir("compact-done-drop-home");
    let cwd = unique_temp_dir("compact-done-drop-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();
    let real = runtime.handle();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = crate::runtime::AgentRuntimeHandle {
        cmd_tx: tx,
        ..real.clone()
    };
    let compact_handle = handle.clone();
    let task = tokio::spawn(async move { compact_handle.compact().await });

    let done = match rx.recv().await.expect("compact command must be sent") {
        crate::agent::orchestrator::OrchCmd::Compact { done } => done,
        _ => panic!("expected an OrchCmd::Compact"),
    };
    // Orchestrator drops the completion sender without reporting a result.
    drop(done);

    let result = task.await.unwrap();
    assert!(
        matches!(result, Err(crate::runtime::RuntimeError::Command(_))),
        "{result:?}"
    );

    // The shared gate must be free again for the real handle.
    let outcome = runtime.run_turn("after done drop").await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn shutdown_ends_compaction_whose_caller_is_gone() {
    let home = unique_temp_dir("compact-shutdown-home");
    let cwd = unique_temp_dir("compact-shutdown-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = seeded_compaction_runtime(&home, &cwd).await;
    let handle = runtime.handle();

    let mut compact = Box::pin(handle.compact());
    assert!(
        tokio::time::timeout(tokio::time::Duration::from_millis(150), &mut compact)
            .await
            .is_err(),
        "compaction should still be running"
    );
    drop(compact);

    let result = tokio::time::timeout(tokio::time::Duration::from_secs(5), runtime.shutdown())
        .await
        .expect("shutdown must finish even when the compaction caller is gone");
    result.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn explicit_stream_cancel_releases_the_turn_gate_after_outcome() {
    let home = unique_temp_dir("mock-cancel-stream-home");
    let cwd = unique_temp_dir("mock-cancel-stream-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let (config, _entered, block) = runtime_config_with_blocking_mock(&home, &cwd);
    let runtime = build_runtime(config).await.unwrap();

    let stream = runtime.stream_turn("blocking").unwrap();
    stream.cancel();
    let cancelled = stream.outcome().await.unwrap();
    assert_eq!(
        cancelled.status,
        crate::agent::orchestrator::TurnStatus::Interrupted
    );
    // Release the backend so the recovery turn can complete promptly.
    block.store(false, std::sync::atomic::Ordering::SeqCst);
    let recovered = runtime.run_turn("recovery").await.unwrap();
    assert_eq!(
        recovered.status,
        crate::agent::orchestrator::TurnStatus::Ok,
        "{:?}",
        recovered.error
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

// ── Mock LLM with tool-use test ───────────────────────────────

/// Mock LLM that exercises the full tool-execution pipeline:
/// first turn returns a Bash tool call, second turn returns Text+Stop.
fn mock_llm_tool_use() -> crate::llm::mock::MockLlmBackend {
    use crate::protocol::{Event, StopEvent, TextEvent, ToolCallEvent};
    use serde_json::json;
    let mut fields = std::collections::BTreeMap::new();
    fields.insert("command".into(), "echo hello".into());
    crate::llm::mock::MockLlmBackend::new(
        "flash",
        vec![
            // First LLM call: request a Bash tool execution
            vec![
                Ok(Event::ToolCall(ToolCallEvent {
                    name: "Bash".into(),
                    id: "call_bash_1".into(),
                    input_json: json!({"command": "echo hello"}),
                    fields,
                    parse_error: None,
                    raw_arguments_digest: None,
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "tool_use".into(),
                })),
            ],
            // Second LLM call: text response after tool execution
            vec![
                Ok(Event::Text(TextEvent {
                    content: "all done".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
        ],
    )
}

struct AsyncEchoTool;

#[async_trait::async_trait]
impl AgentTool for AsyncEchoTool {
    fn definition(&self) -> ToolDefinition {
        let mut definition = ToolDefinition::new(
            "AsyncEcho",
            "Echo text asynchronously",
            serde_json::json!({
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"],
                "additionalProperties": false
            }),
        );
        definition.execution = ToolExecutionMode::ParallelReadOnly;
        definition
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: ToolExecutionContext,
    ) -> Result<ToolOutput, ToolError> {
        if ctx.is_cancelled() {
            return Err(ToolError::new("cancelled"));
        }
        Ok(ToolOutput::text(format!(
            "echo:{}@{}",
            input["text"].as_str().unwrap_or_default(),
            ctx.cwd().display()
        )))
    }
}

struct MutatingEchoTool;

#[async_trait::async_trait]
impl AgentTool for MutatingEchoTool {
    fn definition(&self) -> ToolDefinition {
        let mut definition = ToolDefinition::new(
            "MutatingEcho",
            "A mutating test tool",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        );
        definition.execution = ToolExecutionMode::Sequential;
        definition.mutating = true;
        definition
    }

    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: ToolExecutionContext,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("mutated"))
    }
}

struct CancellableTool;

struct NonCooperativeTool;

#[async_trait::async_trait]
impl AgentTool for NonCooperativeTool {
    fn definition(&self) -> ToolDefinition {
        let mut definition = ToolDefinition::new(
            "NonCooperative",
            "Never completes or checks cancellation",
            serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
        );
        definition.execution = ToolExecutionMode::Sequential;
        definition
    }

    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: ToolExecutionContext,
    ) -> Result<ToolOutput, ToolError> {
        futures::future::pending().await
    }
}

#[async_trait::async_trait]
impl AgentTool for CancellableTool {
    fn definition(&self) -> ToolDefinition {
        let mut definition = ToolDefinition::new(
            "Cancellable",
            "Wait until cancelled",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        );
        definition.execution = ToolExecutionMode::Sequential;
        definition
    }

    async fn execute(
        &self,
        _input: serde_json::Value,
        ctx: ToolExecutionContext,
    ) -> Result<ToolOutput, ToolError> {
        while !ctx.is_cancelled() {
            tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
        }
        Err(ToolError::new("cancelled"))
    }
}

struct DriftingDefinitionTool {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl AgentTool for DriftingDefinitionTool {
    fn definition(&self) -> ToolDefinition {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        ToolDefinition::new(
            if call == 0 {
                "FrozenTool"
            } else {
                "DriftedTool"
            },
            "Definition must be frozen at startup",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        )
    }

    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: ToolExecutionContext,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("frozen"))
    }
}

fn mock_llm_single_tool(name: &str) -> crate::llm::mock::MockLlmBackend {
    use crate::protocol::{Event, StopEvent, TextEvent, ToolCallEvent};
    crate::llm::mock::MockLlmBackend::new(
        "flash",
        vec![
            vec![
                Ok(Event::ToolCall(ToolCallEvent {
                    name: name.into(),
                    id: format!("call_{}", name.to_ascii_lowercase()),
                    input_json: serde_json::json!({}),
                    fields: Default::default(),
                    parse_error: None,
                    raw_arguments_digest: None,
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "tool_use".into(),
                })),
            ],
            vec![
                Ok(Event::Text(TextEvent {
                    content: "done".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
        ],
    )
}

fn mock_llm_custom_tool_use() -> crate::llm::mock::MockLlmBackend {
    use crate::protocol::{Event, StopEvent, TextEvent, ToolCallEvent};
    let mut fields = std::collections::BTreeMap::new();
    fields.insert("text".into(), "hello".into());
    crate::llm::mock::MockLlmBackend::new(
        "flash",
        vec![
            vec![
                Ok(Event::ToolCall(ToolCallEvent {
                    name: "AsyncEcho".into(),
                    id: "call_echo_1".into(),
                    input_json: serde_json::json!({"text": "hello"}),
                    fields,
                    parse_error: None,
                    raw_arguments_digest: None,
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "tool_use".into(),
                })),
            ],
            vec![
                Ok(Event::Text(TextEvent {
                    content: "custom done".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
        ],
    )
}

#[tokio::test]
async fn registered_async_custom_tool_executes_through_the_core_pipeline() {
    let home = unique_temp_dir("custom-tool-home");
    let cwd = unique_temp_dir("custom-tool-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let mut config = runtime_config_with_mock(&home, &cwd, mock_llm_custom_tool_use());
    config.custom_tools.push(Arc::new(AsyncEchoTool));
    let runtime = build_runtime(config).await.unwrap();

    let mut stream = runtime.stream_turn("echo").unwrap();
    let turn_id = stream.turn_id().clone();
    let mut sequences = Vec::new();
    let mut result = None;
    while let Some(event) = stream.recv().await {
        assert_eq!(event.turn_id.as_ref(), Some(&turn_id));
        sequences.push(event.sequence);
        if let crate::runtime::AgentEventKind::ToolResult {
            tool_use_id,
            tool_name,
            content,
            status,
            result_kind,
            ..
        } = event.kind
        {
            result = Some((tool_use_id, tool_name, content, status, result_kind));
        }
    }
    assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
    let outcome = stream.outcome().await.unwrap();
    assert!(outcome.text.contains("custom done"));
    let (tool_use_id, tool_name, content, status, result_kind) = result.unwrap();
    assert_eq!(tool_use_id.as_deref(), Some("call_echo_1"));
    assert_eq!(tool_name, "AsyncEcho");
    assert!(content.contains("echo:hello@"));
    assert!(status.is_success());
    assert_eq!(result_kind, crate::tools::metadata::ToolResultKind::Text);

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn custom_tool_definition_is_evaluated_once_and_frozen() {
    let home = unique_temp_dir("custom-frozen-home");
    let cwd = unique_temp_dir("custom-frozen-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut config = runtime_config_with_mock(&home, &cwd, mock_llm_single_tool("FrozenTool"));
    config.custom_tools.push(Arc::new(DriftingDefinitionTool {
        calls: calls.clone(),
    }));

    let runtime = build_runtime(config).await.unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let outcome = runtime.run_turn("use frozen definition").await.unwrap();
    assert_eq!(outcome.tool_call_count, 1);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn successful_mutating_custom_tool_invalidates_read_memos() {
    let home = unique_temp_dir("custom-mutating-home");
    let cwd = unique_temp_dir("custom-mutating-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let mut config = runtime_config_with_mock(&home, &cwd, mock_llm_single_tool("MutatingEcho"));
    config.custom_tools.push(Arc::new(MutatingEchoTool));
    let runtime = build_runtime(config).await.unwrap();

    assert_eq!(
        runtime
            .ctx
            .memo_mutation
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    runtime.run_turn("mutate").await.unwrap();
    assert_eq!(
        runtime
            .ctx
            .memo_mutation
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn async_custom_tool_observes_stream_cancellation() {
    let home = unique_temp_dir("custom-cancel-home");
    let cwd = unique_temp_dir("custom-cancel-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let mut config = runtime_config_with_mock(&home, &cwd, mock_llm_single_tool("Cancellable"));
    config.custom_tools.push(Arc::new(CancellableTool));
    let runtime = build_runtime(config).await.unwrap();

    let stream = runtime.stream_turn("wait").unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
    stream.cancel();
    let outcome = tokio::time::timeout(tokio::time::Duration::from_secs(2), stream.outcome())
        .await
        .expect("custom tool did not stop after cancellation")
        .unwrap();
    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Interrupted
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn non_cooperative_custom_tool_is_dropped_on_interrupt() {
    let home = unique_temp_dir("custom-non-cooperative-home");
    let cwd = unique_temp_dir("custom-non-cooperative-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let mut config = runtime_config_with_mock(&home, &cwd, mock_llm_single_tool("NonCooperative"));
    config.custom_tools.push(Arc::new(NonCooperativeTool));
    let runtime = build_runtime(config).await.unwrap();

    let stream = runtime.stream_turn("wait").unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(40)).await;
    stream.cancel();
    let outcome = tokio::time::timeout(tokio::time::Duration::from_secs(2), stream.outcome())
        .await
        .expect("non-cooperative custom tool was not dropped")
        .unwrap();
    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Interrupted
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn custom_tool_timeout_is_local_and_next_turn_still_runs() {
    let home = unique_temp_dir("custom-timeout-home");
    let cwd = unique_temp_dir("custom-timeout-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let mut config = runtime_config_with_mock(
        &home,
        &cwd,
        crate::llm::mock::MockLlmBackend::new(
            "flash",
            vec![
                vec![
                    Ok(crate::protocol::Event::ToolCall(
                        crate::protocol::ToolCallEvent {
                            name: "NonCooperative".into(),
                            id: "call-timeout".into(),
                            input_json: serde_json::json!({}),
                            fields: Default::default(),
                            parse_error: None,
                            raw_arguments_digest: None,
                        },
                    )),
                    Ok(crate::protocol::Event::Stop(crate::protocol::StopEvent {
                        reason: "tool_use".into(),
                    })),
                ],
                vec![
                    Ok(crate::protocol::Event::Text(crate::protocol::TextEvent {
                        content: "recovered from tool timeout".into(),
                    })),
                    Ok(crate::protocol::Event::Stop(crate::protocol::StopEvent {
                        reason: "end_turn".into(),
                    })),
                ],
                vec![
                    Ok(crate::protocol::Event::Text(crate::protocol::TextEvent {
                        content: "next turn".into(),
                    })),
                    Ok(crate::protocol::Event::Stop(crate::protocol::StopEvent {
                        reason: "end_turn".into(),
                    })),
                ],
            ],
        ),
    );
    config.config.tool_timeout_secs = 5;
    config.custom_tools.push(Arc::new(NonCooperativeTool));
    let runtime = build_runtime(config).await.unwrap();

    let mut stream = runtime.stream_turn("timeout tool").unwrap();
    let mut timeout_result = None;
    while let Some(event) = stream.recv().await {
        if let crate::runtime::AgentEventKind::ToolResult {
            content, status, ..
        } = event.kind
        {
            timeout_result = Some((content, status));
        }
    }
    let timed_out = stream.outcome().await.unwrap();
    assert_eq!(timed_out.status, crate::agent::orchestrator::TurnStatus::Ok);
    let (content, status) = timeout_result.expect("timeout tool result");
    assert!(!status.is_success());
    assert!(content.contains("timed out after 5s"), "{content}");
    let next = runtime.run_turn("next").await.unwrap();
    assert_eq!(next.status, crate::agent::orchestrator::TurnStatus::Ok);
    assert_eq!(next.text, "next turn");

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn tool_use_turn_executes_tool_and_returns_outcome() {
    let home = unique_temp_dir("mock-tool-home");
    let cwd = unique_temp_dir("mock-tool-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_tool_use()))
        .await
        .unwrap();

    let outcome = runtime.run_turn("run echo hello").await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
    // At least one tool was executed
    assert!(
        outcome.tool_call_count >= 1,
        "expected at least 1 tool call, got {}",
        outcome.tool_call_count
    );
    // The LLM response after tool execution
    assert!(outcome.text.contains("all done"));

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn tool_use_turn_emits_tool_call_and_tool_result_events() {
    let home = unique_temp_dir("mock-tool-ev-home");
    let cwd = unique_temp_dir("mock-tool-ev-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let sink = Arc::new(RecordingSink::default());

    let mut rt_config = runtime_config_with_mock(&home, &cwd, mock_llm_tool_use());
    rt_config.event_sink = Some(sink.clone());

    let runtime = build_runtime(rt_config).await.unwrap();
    runtime.run_turn("run echo hello").await.unwrap();
    runtime.shutdown().await.unwrap();

    let (has_tool_call, has_tool_result, has_final) = {
        let events = sink.events.lock().unwrap();
        (
            events
                .iter()
                .any(|e| matches!(e.kind, crate::runtime::AgentEventKind::ToolCall { .. })),
            events
                .iter()
                .any(|e| matches!(e.kind, crate::runtime::AgentEventKind::ToolResult { .. })),
            events
                .iter()
                .any(|e| matches!(e.kind, crate::runtime::AgentEventKind::Final { .. })),
        )
    };

    assert!(has_tool_call, "expected ToolCall event");
    assert!(has_tool_result, "expected ToolResult event");
    assert!(has_final, "expected Final event");

    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn stream_turn_without_event_sink_emits_tool_events() {
    let home = unique_temp_dir("mock-tool-stream-home");
    let cwd = unique_temp_dir("mock-tool-stream-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_tool_use()))
        .await
        .unwrap();

    let mut stream = runtime.stream_turn("run echo hello").unwrap();
    let mut saw_tool_call = false;
    let mut saw_tool_result = false;
    while let Some(event) =
        tokio::time::timeout(tokio::time::Duration::from_secs(10), stream.recv())
            .await
            .expect("stream event timed out")
    {
        match event.kind {
            crate::runtime::AgentEventKind::ToolCall { .. } => saw_tool_call = true,
            crate::runtime::AgentEventKind::ToolResult { .. } => saw_tool_result = true,
            crate::runtime::AgentEventKind::Final { .. } => break,
            _ => {}
        }
    }

    let outcome = stream.outcome().await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
    assert!(saw_tool_call, "expected streaming ToolCall event");
    assert!(saw_tool_result, "expected streaming ToolResult event");

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

type SeenSystemPrompts = Arc<Mutex<Vec<String>>>;

struct SystemPromptCaptureBackend {
    seen: SeenSystemPrompts,
}

#[async_trait::async_trait]
impl crate::runtime::LlmBackend for SystemPromptCaptureBackend {
    fn name(&self) -> &str {
        "system-prompt-capture"
    }

    async fn stream(
        &self,
        request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        self.seen.lock().unwrap().push(request.system_prompt);
        Ok(crate::runtime::LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![
                Ok(crate::runtime::LlmEvent::Text(
                    crate::runtime::LlmTextEvent {
                        content: "ok".into(),
                    },
                )),
                Ok(crate::runtime::LlmEvent::Stop(
                    crate::runtime::LlmStopEvent {
                        reason: "end_turn".into(),
                    },
                )),
            ])),
            attempt_count: 1,
        })
    }
}

struct StaticPrefixSource {
    system_prompt: String,
}

impl crate::runtime::PrefixSource for StaticPrefixSource {
    fn prefix(&self, _events_path: &std::path::Path) -> Option<(String, Vec<serde_json::Value>)> {
        Some((self.system_prompt.clone(), Vec::new()))
    }
}

#[tokio::test]
async fn prefix_source_overrides_compiled_prefix() {
    let home = unique_temp_dir("prefix-source-home");
    let cwd = unique_temp_dir("prefix-source-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let seen = Arc::new(Mutex::new(Vec::new()));
    let runtime = crate::runtime::AgentRuntime::start(
        crate::runtime::AgentOptions::new(home.clone(), cwd.clone())
            .with_llm_backend(Arc::new(SystemPromptCaptureBackend { seen: seen.clone() }))
            .with_api_key("test-key")
            .with_base_url("https://example.invalid/v1")
            .with_prefix_source(Arc::new(StaticPrefixSource {
                system_prompt: "CUSTOM-PREFIX-SYSTEM-PROMPT".into(),
            })),
    )
    .await
    .unwrap();

    let outcome = runtime.run_turn("hi").await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
    // Second turn in the same process must still consult the source (the
    // compiled prefix cache must never shadow a configured source).
    let outcome = runtime.run_turn("hi again").await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
    let captured = seen.lock().unwrap().clone();
    assert_eq!(
        captured.len(),
        2,
        "both requests must consult the prefix source"
    );
    assert_eq!(captured[0], "CUSTOM-PREFIX-SYSTEM-PROMPT");
    assert_eq!(captured[1], "CUSTOM-PREFIX-SYSTEM-PROMPT");

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

type SeenHookViews = Arc<Mutex<Option<(String, usize, usize, String, String, String, String)>>>;

struct RecordingPostInitHook {
    seen: SeenHookViews,
}

impl crate::runtime::PostInitHook for RecordingPostInitHook {
    fn run(&self, ctx: &crate::runtime::PostInitContext<'_>) -> anyhow::Result<()> {
        *self.seen.lock().unwrap() = Some((
            ctx.system_prompt().to_string(),
            ctx.tools().len(),
            ctx.workflow_ids().len(),
            ctx.workflow_fingerprint().to_string(),
            ctx.tool_surface_fingerprint().to_string(),
            ctx.tool_capabilities_fingerprint().to_string(),
            ctx.dependency_fingerprint().to_string(),
        ));
        assert!(!ctx.cwd().as_os_str().is_empty());
        assert!(!ctx.capabilities().skills.discoverable.is_empty());
        ctx.log_event(serde_json::json!({ "type": "hook_test_event" }))?;
        Ok(())
    }
}

#[tokio::test]
async fn post_init_hook_receives_resolved_view_and_logs_events() {
    let home = unique_temp_dir("post-init-hook-home");
    let cwd = unique_temp_dir("post-init-hook-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let seen_hook = Arc::new(Mutex::new(None));
    let runtime = crate::runtime::AgentRuntime::start(
        crate::runtime::AgentOptions::new(home.clone(), cwd.clone())
            .with_llm_backend(Arc::new(MessageCaptureBackend {
                seen: Arc::new(Mutex::new(Vec::new())),
            }))
            .with_api_key("test-key")
            .with_base_url("https://example.invalid/v1")
            .with_post_init_hook(Arc::new(RecordingPostInitHook {
                seen: seen_hook.clone(),
            })),
    )
    .await
    .unwrap();

    let info = runtime.session_info().clone();
    let view = seen_hook.lock().unwrap().clone().expect("hook ran");
    assert!(
        view.0.contains("system-conventions"),
        "resolved system prompt must carry the core section"
    );
    assert!(view.1 > 0, "tool schemas must be visible to the hook");
    assert!(view.2 > 0, "workflow ids must be visible to the hook");
    assert!(!view.3.is_empty());
    assert!(!view.4.is_empty());
    assert!(!view.5.is_empty());
    assert!(!view.6.is_empty());

    let events_text = tokio::fs::read_to_string(&info.events_path).await.unwrap();
    assert!(
        events_text.contains("\"type\":\"hook_test_event\""),
        "hook event must be persisted"
    );

    let outcome = runtime.run_turn("hi").await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

struct RewritingHook;

impl crate::runtime::PostInitHook for RewritingHook {
    fn run(&self, ctx: &crate::runtime::PostInitContext<'_>) -> anyhow::Result<()> {
        // Simulate a host that rewrites session files before the first turn
        // (session-restructuring hook): append a user line to the
        // conversation and record a prefix_snapshot event.
        let mut conversation =
            std::fs::read_to_string(&ctx.session_paths().conversation).unwrap_or_default();
        conversation.push_str("{\"role\":\"user\",\"content\":\"HOOK-INJECTED-LINE\"}\n");
        std::fs::write(&ctx.session_paths().conversation, conversation)?;
        ctx.log_event(serde_json::json!({
            "type": "prefix_snapshot",
            "version": 1,
            "fingerprint": "hook-test",
            "dependency_fingerprint": "hook-test",
            "system_prompt": "HOOK-PROMPT",
            "tools_json": [],
        }))?;
        Ok(())
    }
}

#[tokio::test]
async fn post_init_hook_rewrites_are_visible_to_first_turn() {
    let home = unique_temp_dir("hook-rewrite-home");
    let cwd = unique_temp_dir("hook-rewrite-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let seen = Arc::new(Mutex::new(Vec::new()));
    let runtime = crate::runtime::AgentRuntime::start(
        crate::runtime::AgentOptions::new(home.clone(), cwd.clone())
            .with_llm_backend(Arc::new(MessageCaptureBackend { seen: seen.clone() }))
            .with_api_key("test-key")
            .with_base_url("https://example.invalid/v1")
            .with_post_init_hook(Arc::new(RewritingHook)),
    )
    .await
    .unwrap();

    let outcome = runtime.run_turn("hi").await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
    let captured = seen.lock().unwrap().clone();
    assert_eq!(captured.len(), 1);
    let messages = &captured[0];
    assert!(
        messages.iter().any(|m| m
            .get("content")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|c| c.contains("HOOK-INJECTED-LINE"))),
        "the hook's conversation rewrite must be visible to the first turn \
         (conversation store cache must not shadow disk rewrites)"
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

/// Latched sessions must be refused before any model request.
struct PanicIfCalledBackend;

#[async_trait::async_trait]
impl crate::llm::client::LlmBackend for PanicIfCalledBackend {
    fn name(&self) -> &str {
        "panic-if-called"
    }

    async fn stream(
        &self,
        _request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        panic!("backend must not be called while the session is latched");
    }
}

/// Emits `retries` control notifications and then stays pending forever.
struct RetryThenPendingBackend {
    retries: usize,
}

#[async_trait::async_trait]
impl crate::llm::client::LlmBackend for RetryThenPendingBackend {
    fn name(&self) -> &str {
        "retry-then-pending"
    }

    async fn stream(
        &self,
        _request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        use crate::protocol::{Event, RetryEvent};
        use futures::StreamExt as _;
        let retries =
            futures::stream::iter((0..self.retries).map(|_| Ok(Event::Retry(RetryEvent {}))));
        Ok(crate::runtime::LlmResponseStream {
            events: Box::pin(retries.chain(futures::stream::pending())),
            attempt_count: 1,
        })
    }
}

#[tokio::test]
async fn public_interrupt_with_writer_recovery_finishes_turn_and_allows_next_turn() {
    use crate::session::event_log::{EventLogWriter, EventLogWriterGate};

    let home = unique_temp_dir("interrupt-critical-home");
    let cwd = unique_temp_dir("interrupt-critical-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let backend = Arc::new(crate::llm::mock::MockLlmBackend::new(
        "flash",
        vec![
            vec![
                Ok(crate::protocol::Event::Text(crate::protocol::TextEvent {
                    content: "first".into(),
                })),
                Ok(crate::protocol::Event::Stop(crate::protocol::StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
            vec![
                Ok(crate::protocol::Event::Text(crate::protocol::TextEvent {
                    content: "second".into(),
                })),
                Ok(crate::protocol::Event::Stop(crate::protocol::StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
        ],
    ));
    let gate = EventLogWriterGate::new();
    let writer = EventLogWriter::start_paused(home.join("stalled-events.jsonl"), gate.clone());

    let config = runtime_config_with_backend(&home, &cwd, backend, |_| {});
    let runtime = crate::runtime::context_build::TEST_EVENT_LOG_WRITER
        .scope(Some(writer.clone()), build_runtime(config))
        .await
        .unwrap();
    let handle = runtime.handle();

    // Stall the writer only after construction: the turn below must then
    // remain inside the critical acknowledgement wait.
    gate.set_paused(true);
    let first = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.run_turn("first").await })
    };

    let mut waited = 0;
    while writer.critical_queued() == 0 && waited < 400 {
        tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
        waited += 1;
    }
    assert!(
        writer.critical_queued() >= 1,
        "the prefix commit must reach the writer queue before we interrupt"
    );

    let started = std::time::Instant::now();
    handle.interrupt_current_turn();
    // Resume the writer right after the interrupt; then this case asserts the
    // turn ends as Interrupted and the next turn runs, not a permanent stall.
    gate.set_paused(false);
    let outcome = tokio::time::timeout(tokio::time::Duration::from_secs(2), first)
        .await
        .expect("interrupt_current_turn must release the stalled critical commit")
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Interrupted,
        "interrupt must classify as Interrupted, not as an event-log error: {:?}",
        outcome.error
    );

    assert!(
        started.elapsed() < std::time::Duration::from_millis(1200),
        "interrupt must not wait for the production deadline: {:?}",
        started.elapsed()
    );
    let second = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        handle.run_turn("second"),
    )
    .await
    .expect("second turn must not hang after recovery")
    .unwrap();
    assert_eq!(
        second.status,
        crate::agent::orchestrator::TurnStatus::Ok,
        "{:?}",
        second.error
    );

    tokio::time::timeout(tokio::time::Duration::from_secs(10), runtime.shutdown())
        .await
        .expect("shutdown must not hang after recovery")
        .unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn shutdown_completes_within_stage_budget_on_real_path() {
    let home = unique_temp_dir("shutdown-budget-home");
    let cwd = unique_temp_dir("shutdown-budget-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_with_usage()))
        .await
        .unwrap();
    let _ = runtime.run_turn("shutdown budget").await.unwrap();

    let started = std::time::Instant::now();
    runtime.shutdown().await.unwrap();

    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "real shutdown flush stages must stay within budget: {:?}",
        started.elapsed()
    );
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

/// Emits `chunks` text deltas of `chunk_bytes` each (bounded-stream tests).
struct TextStormBackend {
    chunks: usize,
    chunk_bytes: usize,
    emitted: Arc<std::sync::atomic::AtomicUsize>,
    finished: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl crate::llm::client::LlmBackend for TextStormBackend {
    fn name(&self) -> &str {
        "text-storm"
    }

    async fn stream(
        &self,
        _request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        use crate::protocol::{Event, StopEvent, TextEvent};
        use futures::StreamExt as _;
        let emitted = self.emitted.clone();
        let finished = self.finished.clone();
        let total = self.chunks;
        let chunk = "x".repeat(self.chunk_bytes);
        let storm = futures::stream::iter(0..self.chunks).map(move |_| {
            if emitted.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 == total {
                finished.notify_waiters();
            }
            Ok(Event::Text(TextEvent {
                content: chunk.clone(),
            }))
        });
        let stop = futures::stream::iter(vec![Ok(Event::Stop(StopEvent {
            reason: "end_turn".into(),
        }))]);
        Ok(crate::runtime::LlmResponseStream {
            events: Box::pin(storm.chain(stop)),
            attempt_count: 1,
        })
    }
}

#[tokio::test]
async fn slow_consumer_bounds_pending_progress_bytes_and_outcome_drains() {
    // Two fixed loads from the Q13 verification spec (8 MiB / 32 MiB).
    for mebibytes in [8usize, 32] {
        let home = unique_temp_dir(&format!("storm-{mebibytes}-home"));
        let cwd = unique_temp_dir(&format!("storm-{mebibytes}-cwd"));
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let chunks = mebibytes * 1024; // 1 KiB per delta
        let backend = Arc::new(TextStormBackend {
            chunks,
            chunk_bytes: 1024,
            emitted: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            finished: Arc::new(tokio::sync::Notify::new()),
        });
        // 32 MiB of deltas also produce 32k diagnostic audit events; this test
        // targets the client-side progress budget, so the audit log is off to
        // avoid queue-loss noise unrelated to the budget under test.
        let runtime = build_runtime(runtime_config_with_backend(
            &home,
            &cwd,
            backend.clone(),
            |cfg| cfg.log_events = false,
        ))
        .await
        .unwrap();

        // Slow consumer: never call recv(); wait until the producer has
        // emitted every delta so the backlog is at its peak.
        let stream = runtime.stream_turn("text storm").unwrap();
        tokio::time::timeout(
            tokio::time::Duration::from_secs(60),
            backend.finished.notified(),
        )
        .await
        .expect("producer must finish the storm");
        let emitted = backend.emitted.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            emitted, chunks,
            "producer must finish the {mebibytes} MiB storm"
        );

        let (pending, dropped) = stream.progress_backlog();
        assert!(
            pending <= 1 << 20,
            "pending progress bytes must stay within the budget at {mebibytes} MiB: {pending}"
        );
        assert!(
            dropped > 0,
            "{mebibytes} MiB of deltas against a 1 MiB budget must drop over-limit deltas"
        );

        // outcome() drains the rest: the result is available and the backlog
        // is released as the stream is consumed.
        let outcome = stream.outcome().await.unwrap();
        assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);

        runtime.shutdown().await.unwrap();
        let _ = tokio::fs::remove_dir_all(home).await;
        let _ = tokio::fs::remove_dir_all(cwd).await;
    }
}

#[tokio::test]
async fn retry_does_not_bypass_first_event_deadline() {
    let home = unique_temp_dir("retry-deadline-home");
    let cwd = unique_temp_dir("retry-deadline-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_backend(
        &home,
        &cwd,
        Arc::new(RetryThenPendingBackend { retries: 1 }),
        |cfg| {
            cfg.llm_first_event_timeout_secs = 1;
            // Single attempt: this test pins the per-attempt deadline, not the
            // retry budget (covered by dedicated recovery tests).
            cfg.llm_recovery.request_max_retries = 0;
            cfg.llm_idle_timeout_secs = 30;
        },
    ))
    .await
    .unwrap();

    let started = std::time::Instant::now();
    let outcome = tokio::time::timeout(
        tokio::time::Duration::from_secs(2),
        runtime.run_turn("retry then silence"),
    )
    .await
    .expect("the first-event deadline must still fire after a Retry event")
    .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Failed
    );
    assert!(
        elapsed < std::time::Duration::from_millis(1_500),
        "Retry must not extend the first-event budget: {elapsed:?}"
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn repeated_retry_events_do_not_extend_first_event_deadline() {
    let home = unique_temp_dir("retry-multi-home");
    let cwd = unique_temp_dir("retry-multi-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_backend(
        &home,
        &cwd,
        Arc::new(RetryThenPendingBackend { retries: 3 }),
        |cfg| {
            cfg.llm_first_event_timeout_secs = 1;
            // Single attempt: this test pins the per-attempt deadline, not the
            // retry budget (covered by dedicated recovery tests).
            cfg.llm_recovery.request_max_retries = 0;
            cfg.llm_idle_timeout_secs = 30;
        },
    ))
    .await
    .unwrap();

    let started = std::time::Instant::now();
    let outcome = tokio::time::timeout(
        tokio::time::Duration::from_secs(2),
        runtime.run_turn("many retries then silence"),
    )
    .await
    .expect("repeated Retry events must not bypass the first-event deadline")
    .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Failed
    );
    assert!(
        elapsed < std::time::Duration::from_millis(1_500),
        "repeated Retry must not extend the first-event budget: {elapsed:?}"
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn retry_followed_by_real_output_still_succeeds() {
    let home = unique_temp_dir("retry-output-home");
    let cwd = unique_temp_dir("retry-output-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    use crate::protocol::{Event, RetryEvent, StopEvent, TextEvent};
    let backend = crate::llm::mock::MockLlmBackend::new(
        "flash",
        vec![vec![
            Ok(Event::Retry(RetryEvent {})),
            Ok(Event::Text(TextEvent {
                content: "after retry".into(),
            })),
            Ok(Event::Stop(StopEvent {
                reason: "end_turn".into(),
            })),
        ]],
    );
    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, backend))
        .await
        .unwrap();

    let outcome = runtime.run_turn("retry then output").await.unwrap();

    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
    assert!(outcome.text.contains("after retry"), "{}", outcome.text);

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn latched_fault_fails_next_turn_without_model_call() {
    let home = unique_temp_dir("latched-turn-home");
    let cwd = unique_temp_dir("latched-turn-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_backend(
        &home,
        &cwd,
        Arc::new(PanicIfCalledBackend),
        |_| {},
    ))
    .await
    .unwrap();
    let _ = runtime
        .ctx
        .persistence_fault
        .raise(std::path::Path::new("todos.json"), "injected fault");

    let outcome = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        runtime.run_turn("plain text must not succeed"),
    )
    .await
    .expect("a latched turn must finish promptly")
    .unwrap();

    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Failed
    );
    assert!(
        outcome
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("persistence fault"),
        "{:?}",
        outcome.error
    );
    assert!(
        runtime.ctx.store.lines().await.unwrap().is_empty(),
        "history must stay untouched for a latched turn"
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

/// Emits a Retry control notification every 5ms forever: the event cadence is
/// faster than the consumer's 25ms tick, which used to starve deadline checks.
struct HighFrequencyRetryBackend;

#[async_trait::async_trait]
impl crate::llm::client::LlmBackend for HighFrequencyRetryBackend {
    fn name(&self) -> &str {
        "high-frequency-retry"
    }

    async fn stream(
        &self,
        _request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        use crate::protocol::{Event, RetryEvent};
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<anyhow::Result<Event>>();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(5));
            loop {
                interval.tick().await;
                if tx.send(Ok(Event::Retry(RetryEvent {}))).is_err() {
                    break;
                }
            }
        });
        let events = futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        Ok(crate::runtime::LlmResponseStream {
            events: Box::pin(events),
            attempt_count: 1,
        })
    }
}

#[tokio::test]
async fn high_frequency_retry_events_do_not_starve_deadline_checks() {
    let home = unique_temp_dir("retry-storm-home");
    let cwd = unique_temp_dir("retry-storm-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_backend(
        &home,
        &cwd,
        Arc::new(HighFrequencyRetryBackend),
        |cfg| {
            cfg.llm_first_event_timeout_secs = 1;
            // Single attempt: this test pins the per-attempt deadline, not the
            // retry budget (covered by dedicated recovery tests).
            cfg.llm_recovery.request_max_retries = 0;
            cfg.llm_idle_timeout_secs = 30;
        },
    ))
    .await
    .unwrap();

    let started = std::time::Instant::now();
    let outcome = tokio::time::timeout(
        tokio::time::Duration::from_secs(2),
        runtime.run_turn("retry storm"),
    )
    .await
    .expect("continuous Retry events must not starve the first-event deadline")
    .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Failed
    );
    assert!(
        elapsed < std::time::Duration::from_millis(1_500),
        "the deadline must fire on the consumption path, not only when idle: {elapsed:?}"
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

/// Custom tool that panics inside its spawned task: this is the deterministic
/// infrastructure failure for the partial-results contract.
struct PanicTool;

#[async_trait::async_trait]
impl AgentTool for PanicTool {
    fn definition(&self) -> ToolDefinition {
        let mut definition = ToolDefinition::new(
            "PanicTool",
            "Always panics (test fixture)",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        );
        definition.execution = ToolExecutionMode::ParallelReadOnly;
        definition
    }

    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: ToolExecutionContext,
    ) -> Result<ToolOutput, ToolError> {
        panic!("panic tool fixture");
    }
}

#[tokio::test]
async fn partial_tool_results_survive_join_failure() {
    let home = unique_temp_dir("partial-fatal-home");
    let cwd = unique_temp_dir("partial-fatal-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    tokio::fs::write(cwd.join("fixture.txt"), "alpha\nbeta\n")
        .await
        .unwrap();
    use crate::protocol::{Event, StopEvent, TextEvent};
    use serde_json::json;
    let mock = crate::llm::mock::MockLlmBackend::new(
        "flash",
        vec![
            vec![
                Ok(Event::ToolCall(tool_call_event(
                    "Read",
                    "call_read",
                    json!({"path":"fixture.txt"}),
                ))),
                Ok(Event::ToolCall(tool_call_event(
                    "PanicTool",
                    "call_panic",
                    json!({}),
                ))),
                Ok(Event::Stop(StopEvent {
                    reason: "tool_use".into(),
                })),
            ],
            vec![
                Ok(Event::Text(TextEvent {
                    content: "should not be reached".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
        ],
    );
    let mut config = runtime_config_with_mock(&home, &cwd, mock);
    config.custom_tools.push(Arc::new(PanicTool));
    let runtime = build_runtime(config).await.unwrap();

    let outcome = runtime.run_turn("read then panic").await.unwrap();
    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Failed,
        "a panicking custom tool must fail the turn: {outcome:?}"
    );
    assert!(
        outcome
            .error
            .as_deref()
            .is_some_and(|error| error.contains("join failed")),
        "fatal cause must surface: {outcome:?}"
    );

    let conversation = tokio::fs::read_to_string(&outcome.session.conversation_path)
        .await
        .unwrap();
    assert!(conversation.contains("call_read"), "{conversation}");
    assert!(
        conversation.contains("alpha"),
        "successful Read result must survive the later failure: {conversation}"
    );
    assert!(
        !conversation.contains("tool execution failed"),
        "no synthetic failure may replace the successful Read: {conversation}"
    );
    assert!(
        conversation.contains("not executed"),
        "the panicked call must be marked as not executed: {conversation}"
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

/// Loopback: two 502s then a 200 through the real OpenAI-compatible
/// backend must issue exactly three physical requests; the runtime attempt
/// loop — not the backend — owns retries.
#[tokio::test]
#[ignore = "requires local loopback sockets"]
async fn builtin_http_retry_uses_exactly_three_requests() {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(AtomicUsize::new(0));
    let seen_server = seen.clone();
    let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"recovered\"}}]}\n\ndata: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}]}\n\ndata: [DONE]\n\n";
    let ok_response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        sse_body.len(),
        sse_body
    );
    let responses = vec![
        "HTTP/1.1 502 Bad Gateway\r\ncontent-length: 11\r\nconnection: close\r\n\r\nbad gateway"
            .to_string(),
        "HTTP/1.1 502 Bad Gateway\r\ncontent-length: 11\r\nconnection: close\r\n\r\nbad gateway"
            .to_string(),
        ok_response,
    ];
    let server = tokio::spawn(async move {
        for response in responses {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let _ = socket.write_all(response.as_bytes()).await;
            seen_server.fetch_add(1, AtomicOrdering::SeqCst);
        }
    });

    let home = unique_temp_dir("http-retry-home");
    let cwd = unique_temp_dir("http-retry-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let cfg = Config {
        model: "flash".into(),
        api_key: "test-key".into(),
        base_url: format!("http://{addr}"),
        max_context_tokens: 1_000_000,
        log_events: true,
        ..Config::default()
    };
    let runtime = build_runtime(AgentRuntimeConfig::from_config(
        cfg,
        home.clone(),
        cwd.clone(),
    ))
    .await
    .unwrap();

    let outcome = runtime.run_turn("retry twice").await.unwrap();
    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Ok,
        "{:?}",
        outcome.error
    );
    assert_eq!(outcome.text, "recovered");
    assert_eq!(
        seen.load(AtomicOrdering::SeqCst),
        3,
        "two retryable failures plus the accepted attempt = three requests"
    );

    server.abort();
    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

struct GuidanceBoundaryBackend {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    requests: Arc<Mutex<Vec<Vec<serde_json::Value>>>>,
}
#[async_trait::async_trait]
impl crate::llm::client::LlmBackend for GuidanceBoundaryBackend {
    fn name(&self) -> &str {
        "guidance-boundary-test"
    }
    async fn stream(
        &self,
        request: crate::llm::client::LlmRequest,
    ) -> anyhow::Result<crate::llm::client::LlmResponseStream> {
        let first = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request.messages);
            requests.len() == 1
        };
        if first {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(crate::llm::client::LlmResponseStream {
            attempt_count: 1,
            events: Box::pin(futures::stream::iter(vec![
                Ok(crate::llm::client::LlmEvent::Text(
                    crate::llm::client::LlmTextEvent {
                        content: if first {
                            "first answer"
                        } else {
                            "guided answer"
                        }
                        .into(),
                    },
                )),
                Ok(crate::llm::client::LlmEvent::Stop(
                    crate::llm::client::LlmStopEvent {
                        reason: "end_turn".into(),
                    },
                )),
            ])),
        })
    }
}
#[tokio::test]
async fn guidance_waits_for_accepted_response_and_continues_same_turn() {
    let home = unique_temp_dir("guidance-home");
    let cwd = unique_temp_dir("guidance-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let backend = Arc::new(GuidanceBoundaryBackend {
        entered: entered.clone(),
        release: release.clone(),
        requests: requests.clone(),
    });
    let runtime = build_runtime(runtime_config_with_backend(&home, &cwd, backend, |_| {}))
        .await
        .unwrap();
    let handle = runtime.handle();
    let input = crate::runtime::HumanInput {
        request_id: "task".into(),
        text: "first task".into(),
        target_turn_id: None,
        attachment_ids: vec![],
    };
    let (receipt, stream) = handle.stream_input(input).unwrap();
    let mut stream = stream.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    let guide = crate::runtime::HumanInput {
        request_id: "guide".into(),
        text: "keep the public API".into(),
        target_turn_id: Some(receipt.turn_id.clone()),
        attachment_ids: vec![],
    };
    let (received, none) = handle.stream_input(guide.clone()).unwrap();
    assert!(none.is_none());
    assert_eq!(received.status, crate::runtime::InputStatus::Pending);
    assert_eq!(
        handle.stream_input(guide).unwrap().0.revision,
        received.revision
    );
    assert_eq!(
        runtime.ctx.store.lines().await.unwrap().len(),
        1,
        "guidance must not mutate an in-flight request"
    );
    release.notify_one();
    while stream.recv().await.is_some() {}
    let outcome = stream.outcome().await.unwrap();
    assert_eq!(outcome.status, crate::runtime::TurnStatus::Ok);
    assert_eq!(requests.lock().unwrap().len(), 2);
    assert!(
        requests.lock().unwrap()[1]
            .iter()
            .any(|message| message["content"] == "keep the public API")
    );
    let history = runtime.ctx.store.lines().await.unwrap();
    assert_eq!(
        history
            .iter()
            .filter(
                |message| message.pointer("/_mink/input_id") == Some(&serde_json::json!("guide"))
            )
            .count(),
        1
    );
    assert_eq!(
        handle
            .input_inbox()
            .entries()
            .iter()
            .filter(|entry| entry.status == crate::runtime::InputStatus::Applied)
            .count(),
        2
    );
    assert!(
        handle
            .stream_input(crate::runtime::HumanInput {
                request_id: "late".into(),
                text: "late".into(),
                target_turn_id: Some(receipt.turn_id),
                attachment_ids: vec![]
            })
            .is_err()
    );
    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}
#[tokio::test]
async fn interrupted_guidance_remains_unapplied_and_explicitly_resumable() {
    let home = unique_temp_dir("guidance-stop-home");
    let cwd = unique_temp_dir("guidance-stop-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = build_runtime(runtime_config_with_backend(
        &home,
        &cwd,
        Arc::new(GuidanceBoundaryBackend {
            entered: entered.clone(),
            release,
            requests: requests.clone(),
        }),
        |_| {},
    ))
    .await
    .unwrap();
    let handle = runtime.handle();
    let (receipt, stream) = handle
        .stream_input(crate::runtime::HumanInput {
            request_id: "task".into(),
            text: "task".into(),
            target_turn_id: None,
            attachment_ids: vec![],
        })
        .unwrap();
    let mut stream = stream.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    handle
        .stream_input(crate::runtime::HumanInput {
            request_id: "guide".into(),
            text: "pending guidance".into(),
            target_turn_id: Some(receipt.turn_id),
            attachment_ids: vec![],
        })
        .unwrap();
    stream.cancel();
    while stream.recv().await.is_some() {}
    assert_eq!(
        stream.outcome().await.unwrap().status,
        crate::runtime::TurnStatus::Interrupted
    );
    let guide = handle
        .input_inbox()
        .entries()
        .into_iter()
        .find(|e| e.input_id == "guide")
        .unwrap();
    assert_eq!(guide.status, crate::runtime::InputStatus::Unapplied);
    let (_, stream) = handle.resume_input("guide", guide.revision).unwrap();
    let mut stream = stream.unwrap();
    while stream.recv().await.is_some() {}
    assert_eq!(
        stream.outcome().await.unwrap().status,
        crate::runtime::TurnStatus::Ok
    );
    assert_eq!(requests.lock().unwrap().len(), 2);
    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

struct BoundaryTool {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}
#[async_trait::async_trait]
impl AgentTool for BoundaryTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "BoundaryTool",
            "Wait for test release",
            serde_json::json!({"type":"object","additionalProperties":false}),
        )
    }
    async fn execute(
        &self,
        _: serde_json::Value,
        _: ToolExecutionContext,
    ) -> Result<ToolOutput, ToolError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(ToolOutput::text("complete tool result"))
    }
}
#[tokio::test]
async fn guidance_during_tool_execution_preserves_exchange_and_round_limit() {
    use crate::protocol::{Event, StopEvent, TextEvent};
    for max_turns in [1, 3] {
        let home = unique_temp_dir("guidance-tool-home");
        let cwd = unique_temp_dir("guidance-tool-cwd");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let backend = crate::llm::mock::MockLlmBackend::new(
            "mock",
            vec![
                vec![
                    Ok(Event::ToolCall(tool_call_event(
                        "BoundaryTool",
                        "boundary",
                        serde_json::json!({}),
                    ))),
                    Ok(Event::Stop(StopEvent {
                        reason: "tool_use".into(),
                    })),
                ],
                vec![
                    Ok(Event::Text(TextEvent {
                        content: "after guide".into(),
                    })),
                    Ok(Event::Stop(StopEvent {
                        reason: "end_turn".into(),
                    })),
                ],
            ],
        );
        let mut config = runtime_config_with_mock(&home, &cwd, backend);
        config.config.max_turns = max_turns;
        config.custom_tools.push(Arc::new(BoundaryTool {
            entered: entered.clone(),
            release: release.clone(),
        }));
        let runtime = build_runtime(config).await.unwrap();
        let handle = runtime.handle();
        let (task, stream) = handle
            .stream_input(crate::runtime::HumanInput {
                request_id: "task".into(),
                text: "task".into(),
                target_turn_id: None,
                attachment_ids: vec![],
            })
            .unwrap();
        let mut stream = stream.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        handle
            .stream_input(crate::runtime::HumanInput {
                request_id: "guide".into(),
                text: "keep API".into(),
                target_turn_id: Some(task.turn_id),
                attachment_ids: vec![],
            })
            .unwrap();
        assert!(
            !runtime
                .ctx
                .store
                .lines()
                .await
                .unwrap()
                .iter()
                .any(|row| row.pointer("/_mink/input_id") == Some(&serde_json::json!("guide")))
        );
        release.notify_one();
        while stream.recv().await.is_some() {}
        let outcome = stream.outcome().await.unwrap();
        let history = runtime.ctx.store.lines().await.unwrap();
        let tool = history
            .iter()
            .position(|row| {
                row["content"]
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_result"))
            })
            .unwrap();
        let receipt = handle
            .input_inbox()
            .entries()
            .into_iter()
            .find(|entry| entry.input_id == "guide")
            .unwrap();
        if max_turns == 1 {
            assert_ne!(outcome.status, crate::runtime::TurnStatus::Ok);
            assert_eq!(receipt.status, crate::runtime::InputStatus::Unapplied);
        } else {
            assert_eq!(outcome.status, crate::runtime::TurnStatus::Ok);
            assert_eq!(receipt.status, crate::runtime::InputStatus::Applied);
            let guide = history
                .iter()
                .position(|row| row.pointer("/_mink/input_id") == Some(&serde_json::json!("guide")))
                .unwrap();
            assert!(tool < guide);
        }
        runtime.shutdown().await.unwrap();
        let _ = tokio::fs::remove_dir_all(home).await;
        let _ = tokio::fs::remove_dir_all(cwd).await;
    }
}

#[tokio::test]
async fn writable_runtime_session_lease_is_exclusive_and_released_on_shutdown() {
    let home = unique_temp_dir("lease-home");
    let cwd = unique_temp_dir("lease-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let options = || {
        AgentOptions::new(home.clone(), cwd.clone())
            .with_session(SessionPolicy::UseOrCreate("lease-test".into()))
    };
    let first = AgentRuntime::start(options()).await.unwrap();
    let second = AgentRuntime::start(options()).await;
    let rejected = second.is_err();
    if let Ok(second) = second {
        second.shutdown().await.unwrap();
    }
    first.shutdown().await.unwrap();
    assert!(
        rejected,
        "two writable runtimes must never own the same session"
    );
    AgentRuntime::start(options())
        .await
        .unwrap()
        .shutdown()
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_missing_review_blocks_without_a_report_repair_round() {
    use crate::runtime::{GoalSpec, GoalStatus};
    let home = unique_temp_dir("goal-missing-home");
    let cwd = unique_temp_dir("goal-missing-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();
    let handle = runtime.handle();
    handle
        .confirmed_test_goal(GoalSpec::new("do work"))
        .unwrap();
    assert!(matches!(
        handle.stream_turn("other"),
        Err(crate::runtime::RuntimeError::Busy { .. })
    ));
    handle.start_goal_driver_and_run().unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, GoalStatus::Blocked);
    assert_eq!(result.iterations.len(), 1);
    assert!(result.iterations.iter().all(|i| i.finished));
    assert_eq!(
        runtime
            .ctx
            .store
            .lines()
            .await
            .unwrap()
            .iter()
            .filter(|m| m["_mink"]["source"] == "goal_continuation")
            .count(),
        1
    );
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_pause_during_request_stops_new_requests_and_preserves_deadline() {
    use crate::runtime::{GoalSpec, GoalStatus};
    let home = unique_temp_dir("goal-pause-home");
    let cwd = unique_temp_dir("goal-pause-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let backend = Arc::new(PendingEstablishBackend::default());
    let runtime =
        AgentRuntime::start(AgentOptions::new(&home, &cwd).with_llm_backend(backend.clone()))
            .await
            .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    let created = handle
        .confirmed_test_goal(GoalSpec::new("do work"))
        .unwrap();
    backend.entered.notified().await;
    let g = handle.goal_snapshot().unwrap();
    handle.pause_goal(&g.id, g.revision).unwrap();
    let paused = tokio::time::timeout(std::time::Duration::from_secs(10), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(paused.status, GoalStatus::Paused);
    assert_eq!(
        paused.spec.limits.deadline_unix_secs,
        created.spec.limits.deadline_unix_secs
    );
    assert_eq!(*backend.calls.lock().unwrap(), 1);
    assert!(handle.pause_goal(&g.id, g.revision).is_err());
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

struct GoalReportBackend {
    calls: std::sync::atomic::AtomicU64,
    continue_first: bool,
}
#[async_trait::async_trait]
impl crate::runtime::LlmBackend for GoalReportBackend {
    fn name(&self) -> &str {
        "goal-reporter"
    }
    async fn stream(
        &self,
        request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let verdict = if self.continue_first
            && mock_review_parts(&request).is_some_and(|(c, _)| c.iteration == 1)
        {
            "needs_work"
        } else {
            "satisfied"
        };
        let events = mock_goal_review(&request, verdict, "");
        Ok(crate::runtime::LlmResponseStream {
            events: Box::pin(futures::stream::iter(events)),
            attempt_count: 1,
        })
    }
}
async fn goal_runtime(
    name: &str,
    continue_first: bool,
    verifier: Option<Arc<dyn crate::runtime::GoalVerifier>>,
) -> (
    AgentRuntime,
    std::path::PathBuf,
    std::path::PathBuf,
    Arc<GoalReportBackend>,
) {
    let home = unique_temp_dir(&format!("{name}-home"));
    let cwd = unique_temp_dir(&format!("{name}-cwd"));
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("proof.txt"), "objective evidence").unwrap();
    let backend = Arc::new(GoalReportBackend {
        calls: Default::default(),
        continue_first,
    });
    let mut options = AgentOptions::new(&home, &cwd).with_llm_backend(backend.clone());
    if let Some(verifier) = verifier {
        options = options.with_goal_verifier(verifier);
    }
    let runtime = AgentRuntime::start(options).await.unwrap();
    (runtime, home, cwd, backend)
}
/// Reports `blocked` on every goal round so the streak gate can be observed.
struct AlwaysBlockedBackend {
    calls: std::sync::atomic::AtomicU64,
}
#[async_trait::async_trait]
impl crate::runtime::LlmBackend for AlwaysBlockedBackend {
    fn name(&self) -> &str {
        "always-blocked"
    }
    async fn stream(
        &self,
        request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let events = mock_goal_review(&request, "blocked", "waiting on human prerequisite");
        Ok(crate::runtime::LlmResponseStream {
            events: Box::pin(futures::stream::iter(events)),
            attempt_count: 1,
        })
    }
}

#[tokio::test]
async fn model_reported_block_needs_consecutive_rounds_before_ending_the_goal() {
    use crate::runtime::*;
    let home = unique_temp_dir("goal-blocked-home");
    let cwd = unique_temp_dir("goal-blocked-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        cwd.join("proof.txt"),
        "human prerequisite remains unavailable",
    )
    .unwrap();
    let backend = Arc::new(AlwaysBlockedBackend {
        calls: Default::default(),
    });
    let runtime =
        AgentRuntime::start(AgentOptions::new(&home, &cwd).with_llm_backend(backend.clone()))
            .await
            .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    let created = handle
        .confirmed_test_goal(GoalSpec::new("stuck task"))
        .unwrap();
    let mut limits = created.spec.limits.clone();
    limits.max_turns = 10;
    handle
        .update_goal_limits(&created.id, created.revision, limits)
        .unwrap();

    let goal = tokio::time::timeout(std::time::Duration::from_secs(30), handle.wait_goal())
        .await
        .expect("driver must terminate")
        .unwrap();

    // A model cannot end the goal by declaring a blocker on its first round:
    // the same condition has to persist across several rounds, otherwise one
    // unlucky turn would discard the remaining budget.
    assert_eq!(goal.status, GoalStatus::Blocked, "{goal:?}");
    assert!(
        goal.blocked_rounds >= 3,
        "blocked after only {} rounds",
        goal.blocked_rounds
    );
    assert!(
        goal.blocker_rounds >= 3,
        "the same blocker must reach the gate: {}",
        goal.blocker_rounds
    );
    assert!(
        goal.iterations.len() >= 3,
        "streak must span distinct rounds, got {}",
        goal.iterations.len()
    );
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

/// Reports a scripted sequence of goal reports: even provider calls carry the
/// report of round `n/2`, odd calls finish that round.
struct ScriptedReportBackend {
    calls: std::sync::atomic::AtomicU64,
    script: Vec<(&'static str, String, String)>,
}
#[async_trait::async_trait]
impl crate::runtime::LlmBackend for ScriptedReportBackend {
    fn name(&self) -> &str {
        "scripted-goal-reports"
    }
    async fn stream(
        &self,
        request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let index = mock_review_parts(&request)
            .map_or(0, |(c, _)| c.iteration.saturating_sub(1) as usize)
            .min(self.script.len() - 1);
        let (kind, _summary, dependency) = &self.script[index];
        let verdict = match *kind {
            "complete" => "satisfied",
            "blocked" => "blocked",
            _ => "needs_work",
        };
        let events = mock_goal_review(&request, verdict, dependency);
        Ok(crate::runtime::LlmResponseStream {
            events: Box::pin(futures::stream::iter(events)),
            attempt_count: 1,
        })
    }
}

/// Audit9 F2: different blockers must not be accumulated into one streak. The
/// gate counts consecutive reports of the *same* condition, while
/// `blocked_rounds` keeps counting the consecutive blocked rounds.
#[tokio::test]
async fn audit9_different_blockers_do_not_share_a_streak() {
    use crate::runtime::*;
    let home = unique_temp_dir("goal-blocked-changing-home");
    let cwd = unique_temp_dir("goal-blocked-changing-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        cwd.join("proof.txt"),
        "independent prerequisites remain unavailable",
    )
    .unwrap();
    let backend = Arc::new(ScriptedReportBackend {
        calls: Default::default(),
        script: (0..10)
            .map(|n| {
                (
                    "blocked",
                    format!("missing independent prerequisite {n}"),
                    format!("supply prerequisite {n}"),
                )
            })
            .collect(),
    });
    let runtime = AgentRuntime::start(AgentOptions::new(&home, &cwd).with_llm_backend(backend))
        .await
        .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    let created = handle
        .confirmed_test_goal(GoalSpec::new("stuck task"))
        .unwrap();
    let mut limits = created.spec.limits.clone();
    limits.max_turns = 10;
    handle
        .update_goal_limits(&created.id, created.revision, limits)
        .unwrap();

    let goal = tokio::time::timeout(std::time::Duration::from_secs(30), handle.wait_goal())
        .await
        .expect("driver must terminate")
        .unwrap();
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&cwd);

    assert_eq!(
        goal.status,
        GoalStatus::BudgetExhausted,
        "different blockers must not end the goal: {goal:?}"
    );
    assert!(
        goal.blocked_rounds >= 3,
        "consecutive blocked reports: {}",
        goal.blocked_rounds
    );
    assert_eq!(
        goal.blocker_rounds, 1,
        "a changed blocker identity must restart the gate streak"
    );
    assert!(
        goal.iterations.len() >= 3,
        "streak must span distinct rounds, got {}",
        goal.iterations.len()
    );
    assert_eq!(
        goal.budget().blocker_rounds,
        1,
        "the budget view shows the same-blocker streak"
    );
    assert_eq!(
        goal.budget().blocked_rounds,
        goal.blocked_rounds,
        "the budget's blocked_rounds mirrors the snapshot's consecutive count"
    );
}

/// Audit9 F2: `Continue` clears the blocker identity and its streak, so the
/// next blocked report starts a fresh count.
#[tokio::test]
async fn audit9_continue_clears_the_blocker_streak() {
    use crate::runtime::*;
    let home = unique_temp_dir("goal-blocked-continue-home");
    let cwd = unique_temp_dir("goal-blocked-continue-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        cwd.join("proof.txt"),
        "human prerequisite remains unavailable",
    )
    .unwrap();
    let backend = Arc::new(ScriptedReportBackend {
        calls: Default::default(),
        script: vec![
            ("blocked", "same blocker A".into(), "do A".into()),
            ("blocked", "same blocker A".into(), "do A".into()),
            ("continue", "progress".into(), "keep going".into()),
            ("blocked", "same blocker A".into(), "do A".into()),
        ],
    });
    let runtime = AgentRuntime::start(AgentOptions::new(&home, &cwd).with_llm_backend(backend))
        .await
        .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    let created = handle
        .confirmed_test_goal(GoalSpec::new("stuck task"))
        .unwrap();
    let mut limits = created.spec.limits.clone();
    limits.max_turns = 4;
    handle
        .update_goal_limits(&created.id, created.revision, limits)
        .unwrap();

    let goal = tokio::time::timeout(std::time::Duration::from_secs(30), handle.wait_goal())
        .await
        .expect("driver must terminate")
        .unwrap();
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&cwd);

    assert_eq!(goal.status, GoalStatus::BudgetExhausted, "{goal:?}");
    assert_eq!(
        goal.blocker_rounds, 1,
        "Continue must clear the streak identity and count"
    );
    assert_eq!(goal.blocked_rounds, 1);
}

/// Audit9 F2: the same-blocker streak and its identity survive a runtime
/// restart, so recovery cannot silently reset the gate.
#[tokio::test]
async fn audit9_blocker_streak_survives_restart() {
    use crate::runtime::*;
    let home = unique_temp_dir("goal-blocked-restore-home");
    let cwd = unique_temp_dir("goal-blocked-restore-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        cwd.join("proof.txt"),
        "human prerequisite remains unavailable",
    )
    .unwrap();
    let backend = Arc::new(AlwaysBlockedBackend {
        calls: Default::default(),
    });
    let runtime = AgentRuntime::start(AgentOptions::new(&home, &cwd).with_llm_backend(backend))
        .await
        .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    handle
        .confirmed_test_goal(GoalSpec::new("stuck task"))
        .unwrap();
    let sid = handle.session_info().session_id.clone();

    // Let the first blocked rounds settle, then restart the runtime while the
    // goal is still active.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let snapshot = handle.goal_snapshot().unwrap();
        if snapshot.blocker_rounds >= 2 || snapshot.status == GoalStatus::Blocked {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "goal never reached two same-blocker rounds: {snapshot:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    runtime.shutdown().await.unwrap();

    let backend = Arc::new(AlwaysBlockedBackend {
        calls: Default::default(),
    });
    let runtime = AgentRuntime::start(
        AgentOptions::new(&home, &cwd)
            .with_session(SessionPolicy::Resume(sid))
            .with_llm_backend(backend),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    let goal = tokio::time::timeout(std::time::Duration::from_secs(30), handle.wait_goal())
        .await
        .expect("driver must terminate")
        .unwrap();
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&cwd);

    assert_eq!(goal.status, GoalStatus::Blocked, "{goal:?}");
    assert!(
        goal.blocker_rounds >= 3,
        "the streak must survive recovery: {}",
        goal.blocker_rounds
    );
    assert!(goal.blocked_rounds >= 3);
    let _ = goal.id;
}

/// Audit9 F1: the other legal ordering — acceptance commits first, a later
/// stop command is a rejected terminal command and must not latch a fault.
#[tokio::test]
async fn audit9_stop_after_commit_is_rejected_without_a_fault() {
    use crate::runtime::*;
    let (runtime, home, cwd, _) = goal_runtime("audit9-commit-first", false, None).await;
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    handle
        .confirmed_test_goal(GoalSpec::new("produce proof"))
        .unwrap();
    let completed = tokio::time::timeout(std::time::Duration::from_secs(30), handle.wait_goal())
        .await
        .expect("driver must terminate")
        .unwrap();
    assert_eq!(completed.status, GoalStatus::Completed, "{completed:?}");
    let refused = handle.pause_goal(&completed.id, completed.revision);
    assert!(
        refused.is_err(),
        "a terminal goal cannot be paused: {refused:?}"
    );
    assert!(
        matches!(
            handle.goal_driver.state(),
            crate::runtime::goal::DriverState::Idle
        ),
        "a rejected command must not latch a fault: {:?}",
        handle.goal_driver.state()
    );
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_natural_end_continues_then_completes_with_independent_review() {
    use crate::runtime::*;
    let (runtime, home, cwd, backend) = goal_runtime("goal-complete", true, None).await;
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    handle
        .confirmed_test_goal(GoalSpec::new("produce proof"))
        .unwrap();
    let goal = handle.wait_goal().await.unwrap();
    assert_eq!(goal.status, GoalStatus::Completed, "{goal:?}");
    assert_eq!(
        goal.completion_basis,
        Some(GoalCompletionBasis::AgentReviewed)
    );
    assert_eq!(goal.iterations.len(), 2);
    assert_eq!(backend.calls.load(std::sync::atomic::Ordering::SeqCst), 6);
    assert_eq!(goal.usage.request_count, 6);
    assert_eq!(goal.usage.unreported_request_count, 6);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

struct RetryableReviewBackend {
    review_calls: std::sync::atomic::AtomicU64,
    execution_calls: std::sync::atomic::AtomicU64,
}
#[async_trait::async_trait]
impl crate::runtime::LlmBackend for RetryableReviewBackend {
    fn name(&self) -> &str {
        "retryable-goal-review"
    }
    async fn stream(
        &self,
        request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        let independent = request
            .messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .next_back()
            .is_some_and(|s| s.starts_with("<independent-goal-review>"));
        let events = if independent {
            if self
                .review_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                == 0
            {
                anyhow::bail!("independent review backend unavailable")
            }
            natural_goal_end()
        } else if mock_review_parts(&request).is_some() {
            mock_goal_review(&request, "satisfied", "")
        } else {
            self.execution_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            natural_goal_end()
        };
        Ok(crate::runtime::LlmResponseStream {
            events: Box::pin(futures::stream::iter(events)),
            attempt_count: 1,
        })
    }
}

#[tokio::test]
async fn goal_review_failure_can_resume_same_candidate_without_reexecuting() {
    use crate::runtime::*;
    let home = unique_temp_dir("goal-review-retry-home");
    let cwd = unique_temp_dir("goal-review-retry-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("proof.txt"), "objective evidence").unwrap();
    let backend = Arc::new(RetryableReviewBackend {
        review_calls: Default::default(),
        execution_calls: Default::default(),
    });
    let runtime =
        AgentRuntime::start(AgentOptions::new(&home, &cwd).with_llm_backend(backend.clone()))
            .await
            .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    let mut spec = GoalSpec::new("produce proof");
    spec.limits.max_turns = 1;
    handle.confirmed_test_goal(spec).unwrap();

    let blocked = tokio::time::timeout(std::time::Duration::from_secs(15), handle.wait_goal())
        .await
        .expect("first review should finish with an unavailable result")
        .unwrap();
    assert_eq!(blocked.status, GoalStatus::Blocked, "{blocked:?}");
    assert_eq!(blocked.iterations.len(), 1);
    assert_eq!(blocked.remaining_turns(), 0);
    assert!(
        blocked
            .review_failure_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("independent review backend unavailable"))
    );
    let candidate = blocked.review_candidate.clone().expect("retry candidate");

    let resumed = handle.resume_goal(&blocked.id, blocked.revision).unwrap();
    assert_eq!(resumed.phase, GoalPhase::Verifying);
    assert_eq!(resumed.review_candidate.as_ref(), Some(&candidate));
    assert_eq!(resumed.review_failure_reason, None);
    let completed = tokio::time::timeout(std::time::Duration::from_secs(15), handle.wait_goal())
        .await
        .expect("the resumed audit should complete")
        .unwrap();
    assert_eq!(completed.status, GoalStatus::Completed, "{completed:?}");
    assert_eq!(
        completed.iterations.len(),
        1,
        "resume must not run the agent again"
    );
    assert_eq!(
        backend
            .execution_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "review retry must reuse the settled execution round"
    );
    assert!(
        backend
            .review_calls
            .load(std::sync::atomic::Ordering::SeqCst)
            >= 2
    );

    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

struct HostSubmissionRecoveryBackend {
    review_calls: std::sync::atomic::AtomicU64,
    execution_calls: std::sync::atomic::AtomicU64,
    recover_after_rejections: bool,
    end_without_conclusion: bool,
}
#[async_trait::async_trait]
impl crate::runtime::LlmBackend for HostSubmissionRecoveryBackend {
    fn name(&self) -> &str {
        "format-recovering-goal-review"
    }
    async fn stream(
        &self,
        request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        let events = if mock_review_parts(&request).is_some() {
            let call = self
                .review_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let continued = request
                .messages
                .iter()
                .filter_map(|m| m["content"].as_str())
                .any(|s| s.starts_with("<goal-conclusion-required>"))
                || call >= 4;
            if continued && self.recover_after_rejections {
                mock_goal_review(&request, "satisfied", "")
            } else if call == 0 && self.end_without_conclusion {
                vec![
                    Ok(LlmEvent::Text(LlmTextEvent {
                        content: "Some evidence paths were only found with Glob; I will inspect them with Read.".into(),
                    })),
                    Ok(LlmEvent::Stop(LlmStopEvent {
                        reason: "end_turn".into(),
                    })),
                ]
            } else {
                let mut events = mock_goal_review(&request, "satisfied", "");
                events.retain(|event| {
                    !matches!(event, Ok(LlmEvent::ToolCall(tool_call)) if tool_call.name == "Read")
                });
                for event in &mut events {
                    if let Ok(LlmEvent::ToolCall(tool_call)) = event
                        && tool_call.name == "GoalReview"
                    {
                        tool_call.id = format!("invalid-review-{call}");
                        if !self.recover_after_rejections {
                            tool_call.parse_error = Some("invalid host tool-call JSON".into());
                        }
                    }
                }
                events
            }
        } else if request
            .messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .next_back()
            .is_some_and(|s| s.starts_with("<independent-goal-review>"))
        {
            natural_goal_end()
        } else {
            self.execution_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            natural_goal_end()
        };
        Ok(crate::runtime::LlmResponseStream {
            events: Box::pin(futures::stream::iter(events)),
            attempt_count: 1,
        })
    }
}

#[tokio::test]
async fn goal_host_corrects_unread_evidence_after_repeated_submission_rejections() {
    use crate::runtime::*;
    let home = unique_temp_dir("goal-review-format-continuation-home");
    let cwd = unique_temp_dir("goal-review-format-continuation-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("proof.txt"), "objective evidence").unwrap();
    let backend = Arc::new(HostSubmissionRecoveryBackend {
        review_calls: Default::default(),
        execution_calls: Default::default(),
        recover_after_rejections: true,
        end_without_conclusion: false,
    });
    let runtime =
        AgentRuntime::start(AgentOptions::new(&home, &cwd).with_llm_backend(backend.clone()))
            .await
            .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    let mut spec = GoalSpec::new("produce proof");
    spec.limits.max_turns = 1;
    handle.confirmed_test_goal(spec).unwrap();

    let completed = tokio::time::timeout(std::time::Duration::from_secs(15), handle.wait_goal())
        .await
        .expect("host correction should complete within the shared deadline")
        .unwrap();
    assert_eq!(completed.status, GoalStatus::Completed, "{completed:?}");
    assert_eq!(
        completed.iterations.len(),
        1,
        "review must not rerun execution"
    );
    assert_eq!(completed.reviews.len(), 1);
    assert_eq!(
        backend
            .execution_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "host correction does not replay the execution round"
    );
    assert_eq!(
        backend
            .review_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        5,
        "four unread-evidence rejections are followed by a corrected host submission"
    );

    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_host_reports_terminal_reason_after_malformed_tool_calls() {
    use crate::runtime::*;
    let home = unique_temp_dir("goal-review-format-failure-home");
    let cwd = unique_temp_dir("goal-review-format-failure-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("proof.txt"), "objective evidence").unwrap();
    let backend = Arc::new(HostSubmissionRecoveryBackend {
        review_calls: Default::default(),
        execution_calls: Default::default(),
        recover_after_rejections: false,
        end_without_conclusion: false,
    });
    let runtime =
        AgentRuntime::start(AgentOptions::new(&home, &cwd).with_llm_backend(backend.clone()))
            .await
            .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    let mut spec = GoalSpec::new("produce proof");
    spec.limits.max_turns = 1;
    handle.confirmed_test_goal(spec).unwrap();

    let blocked = tokio::time::timeout(std::time::Duration::from_secs(15), handle.wait_goal())
        .await
        .expect("the bounded host turn should finish")
        .unwrap();
    assert_eq!(blocked.status, GoalStatus::Blocked, "{blocked:?}");
    let reason = blocked.review_failure_reason.as_deref().unwrap_or_default();
    assert!(reason.contains("format_recovery_exhausted"), "{reason}");
    assert!(reason.contains("invalid host tool-call JSON"), "{reason}");
    assert!(!reason.contains("Ok(Err("), "{reason}");
    assert_eq!(
        backend
            .review_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        4,
        "malformed host calls still obey the configured format window"
    );

    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_host_continues_after_prose_end_without_submitting_conclusion() {
    use crate::runtime::*;
    let home = unique_temp_dir("goal-review-empty-end-home");
    let cwd = unique_temp_dir("goal-review-empty-end-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("proof.txt"), "objective evidence").unwrap();
    let backend = Arc::new(HostSubmissionRecoveryBackend {
        review_calls: Default::default(),
        execution_calls: Default::default(),
        recover_after_rejections: true,
        end_without_conclusion: true,
    });
    let runtime =
        AgentRuntime::start(AgentOptions::new(&home, &cwd).with_llm_backend(backend.clone()))
            .await
            .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    let mut spec = GoalSpec::new("produce proof");
    spec.limits.max_turns = 1;
    handle.confirmed_test_goal(spec).unwrap();

    let completed = tokio::time::timeout(std::time::Duration::from_secs(15), handle.wait_goal())
        .await
        .expect("host correction should complete within the shared deadline")
        .unwrap();
    assert_eq!(completed.status, GoalStatus::Completed, "{completed:?}");
    assert_eq!(
        completed.iterations.len(),
        1,
        "review must not rerun execution"
    );
    assert_eq!(completed.reviews.len(), 1);
    assert_eq!(
        backend
            .execution_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "host correction does not replay the execution round"
    );
    assert_eq!(
        backend
            .review_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the host's prose end is followed by an internal reminder and a tool submission"
    );

    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_human_approval_checks_evidence_versions() {
    use crate::runtime::*;
    let (runtime, home, cwd, _) = goal_runtime("goal-human", false, None).await;
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    let mut spec = GoalSpec::new("produce proof");
    spec.acceptance = GoalAcceptance::Human;
    handle.confirmed_test_goal(spec).unwrap();
    let goal = handle.wait_goal().await.unwrap();
    assert_eq!(goal.status, GoalStatus::Blocked);
    std::fs::write(
        cwd.join("proof.txt"),
        "changed since completion application",
    )
    .unwrap();
    assert!(
        handle
            .approve_goal(&goal.id, goal.revision, vec!["reviewed".into()])
            .is_err(),
        "approval must reject changed evidence"
    );
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}
struct TestGoalVerifier {
    status: crate::runtime::GoalVerificationStatus,
    mutate: bool,
}
#[async_trait::async_trait]
impl crate::runtime::GoalVerifier for TestGoalVerifier {
    fn id(&self) -> &str {
        "test-check-v1"
    }
    async fn verify(
        &self,
        context: crate::runtime::GoalVerificationContext,
    ) -> anyhow::Result<crate::runtime::GoalVerification> {
        if self.mutate {
            std::fs::write(
                context.cwd.join("proof.txt"),
                format!("changed {}", context.goal.revision),
            )?;
        }
        Ok(crate::runtime::GoalVerification {
            status: self.status.clone(),
            reason: "check result".into(),
            evidence: vec!["proof.txt".into()],
        })
    }
}
#[tokio::test]
async fn goal_verifier_failure_stalls_and_missing_verifier_never_downgrades() {
    use crate::runtime::*;
    for missing in [false, true] {
        let verifier = if missing {
            None
        } else {
            Some(Arc::new(TestGoalVerifier {
                status: GoalVerificationStatus::Fail,
                mutate: false,
            }) as Arc<dyn GoalVerifier>)
        };
        let (runtime, home, cwd, backend) = goal_runtime("goal-verifier", false, verifier).await;
        let handle = runtime.handle();
        handle.start_goal_driver_and_run().unwrap();
        let mut spec = GoalSpec::new("produce proof");
        spec.acceptance = GoalAcceptance::Verifier {
            id: "test-check-v1".into(),
        };
        handle.confirmed_test_goal(spec).unwrap();
        let goal = handle.wait_goal().await.unwrap();
        assert_eq!(goal.status, GoalStatus::Blocked);
        assert_eq!(goal.completion_basis, None);
        assert_eq!(goal.iterations.len(), if missing { 0 } else { 3 });
        assert_eq!(
            backend.calls.load(std::sync::atomic::Ordering::SeqCst),
            if missing { 0 } else { 9 }
        );
        runtime.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(cwd);
    }
}

#[tokio::test]
async fn session_lease_stays_owned_until_outstanding_tool_context_exits() {
    let home = unique_temp_dir("lease-tool-home");
    let cwd = unique_temp_dir("lease-tool-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let options = || AgentOptions::new(&home, &cwd);
    let runtime = AgentRuntime::start(options()).await.unwrap();
    let running_tool = crate::context::ToolContext::from(runtime.ctx.as_ref());
    runtime.shutdown().await.unwrap();
    let second = AgentRuntime::start(options()).await;
    let rejected = second.is_err();
    if let Ok(second) = second {
        second.shutdown().await.unwrap();
    }
    assert!(
        rejected,
        "an outstanding tool must keep the lease even after runtime teardown"
    );
    drop(running_tool);
    AgentRuntime::start(options())
        .await
        .unwrap()
        .shutdown()
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_review_can_read_its_own_spilled_contract_artifact() {
    use crate::runtime::*;
    struct SpillReader(std::sync::atomic::AtomicUsize);
    #[async_trait::async_trait]
    impl LlmBackend for SpillReader {
        fn name(&self) -> &str {
            "review-spill-reader"
        }
        async fn stream(&self, request: LlmRequest) -> anyhow::Result<LlmResponseStream> {
            let events = if mock_review_parts(&request).is_none() {
                natural_goal_end()
            } else {
                let stage = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if stage < 2 {
                    let (name, input) = if stage == 0 {
                        ("GoalRead", serde_json::json!({}))
                    } else {
                        let serialized = serde_json::to_string(request.messages.last().unwrap())?;
                        let suffix = serialized
                            .split("artifact://")
                            .nth(1)
                            .expect("GoalRead must spill");
                        let id: String = suffix
                            .chars()
                            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                            .collect();
                        (
                            "Read",
                            serde_json::json!({"path":format!("artifact://{id}")}),
                        )
                    };
                    vec![
                        Ok(LlmEvent::ToolCall(crate::protocol::ToolCallEvent {
                            id: format!("spill-{stage}"),
                            name: name.into(),
                            input_json: input,
                            fields: Default::default(),
                            parse_error: None,
                            raw_arguments_digest: None,
                        })),
                        Ok(LlmEvent::Stop(LlmStopEvent {
                            reason: "tool_use".into(),
                        })),
                    ]
                } else {
                    mock_goal_review(&request, "satisfied", "")
                }
            };
            Ok(LlmResponseStream {
                events: Box::pin(futures::stream::iter(events)),
                attempt_count: 1,
            })
        }
    }
    let home = unique_temp_dir("review-spill-home");
    let cwd = unique_temp_dir("review-spill-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("proof.txt"), "objective evidence").unwrap();
    let runtime = AgentRuntime::start(
        AgentOptions::new(&home, &cwd)
            .with_tool_result_max_bytes(512)
            .with_llm_backend(Arc::new(SpillReader(Default::default()))),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let mut spec = crate::runtime::GoalSpec::new("produce proof");
    spec.limits.max_turns = 1;
    handle.confirmed_test_goal(spec).unwrap();
    handle.start_goal_driver_and_run().unwrap();
    let goal = handle.wait_goal().await.unwrap();
    let artifacts: Vec<_> = goal
        .evidence_versions
        .iter()
        .filter(|(r, _)| r.starts_with("artifact://"))
        .collect();
    let imported = artifacts
        .iter()
        .map(|(r, _)| {
            runtime
                .ctx
                .artifacts
                .read_text(r.strip_prefix("artifact://").unwrap())
        })
        .collect::<anyhow::Result<Vec<_>>>();
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    assert_eq!(
        goal.status,
        crate::runtime::GoalStatus::Completed,
        "{:?}",
        goal.reason
    );
    assert_eq!(artifacts.len(), 1);
    assert!(imported.unwrap()[0].contains("produce proof"));
    assert_eq!(goal.iterations.len(), 1);
}

#[tokio::test]
async fn goal_known_checker_result_closes_marker_when_evidence_cannot_be_hashed() {
    use crate::runtime::*;
    struct MakeDirectory;
    #[async_trait::async_trait]
    impl GoalVerifier for MakeDirectory {
        fn id(&self) -> &str {
            "directory-result"
        }
        async fn verify(
            &self,
            context: GoalVerificationContext,
        ) -> anyhow::Result<GoalVerification> {
            let path = context.cwd.join("proof.txt");
            std::fs::remove_file(&path)?;
            std::fs::create_dir(path)?;
            Ok(GoalVerification {
                status: GoalVerificationStatus::Pass,
                reason: "known checker result".into(),
                evidence: vec![],
            })
        }
    }
    let (runtime, home, cwd, _) = goal_runtime(
        "goal-checker-evidence-error",
        false,
        Some(Arc::new(MakeDirectory)),
    )
    .await;
    let handle = runtime.handle();
    handle
        .confirmed_test_goal(GoalSpec::new("produce proof"))
        .unwrap();
    handle.start_goal_driver_and_run().unwrap();
    let goal = handle.wait_goal().await.unwrap();
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    assert_eq!(goal.status, GoalStatus::Blocked);
    assert!(
        goal.pending_effects.is_empty(),
        "the checker returned a known result"
    );
    assert_eq!(goal.checker_records.len(), 1);
    assert_eq!(goal.checker_records[0].status, GoalCriterionStatus::Pass);
    assert!(goal.checker_passed_candidate.is_none());
}

#[tokio::test]
async fn goal_recovery_reaudits_known_checker_changes_without_replaying_checker() {
    use crate::runtime::*;
    struct MustNotReplay;
    #[async_trait::async_trait]
    impl GoalVerifier for MustNotReplay {
        fn id(&self) -> &str {
            "known-checker"
        }
        async fn verify(&self, _: GoalVerificationContext) -> anyhow::Result<GoalVerification> {
            anyhow::bail!("a durably known unchanged checker result must not be replayed")
        }
    }
    let verifier = Arc::new(MustNotReplay);
    let (runtime, home, cwd, _) =
        goal_runtime("goal-checker-recovery", false, Some(verifier.clone())).await;
    let handle = runtime.handle();
    let mut spec = GoalSpec::new("produce proof");
    spec.limits.max_turns = 1;
    handle.confirmed_test_goal(spec).unwrap();
    let sid = handle.session_info().session_id.clone();
    let executing = runtime
        .ctx
        .goal_store
        .begin_iteration("committed-execution", 0)
        .unwrap();
    let old_hash = handle.evidence_version("proof.txt").unwrap();
    std::fs::write(cwd.join("proof.txt"), "known checker refreshed proof").unwrap();
    let new_hash = handle.evidence_version("proof.txt").unwrap();
    let candidate = GoalReviewCandidate {
        generation: 1,
        goal_id: executing.id.clone(),
        contract_revision: executing.contract_revision,
        iteration: 1,
        conversation_seq: 0,
        reviewer_id: format!(
            "goal-review:{}:{}",
            runtime.ctx.active_model.lock().unwrap().actual,
            runtime.ctx.tool_surface.fingerprint()
        ),
    };
    let review: GoalReview = serde_json::from_value(serde_json::json!({
        "candidate":candidate,"verdict":"satisfied",
        "original_request":{"status":"pass","reason":"checked","evidence":["proof.txt"]},
        "criteria":[{"criterion_id":"criterion-1","status":"pass","reason":"checked","evidence":["proof.txt"]}],
        "constraints":[],"summary":"checked","next_step":"","blocker":null
    })).unwrap();
    runtime
        .ctx
        .goal_store
        .change(&executing.id, executing.revision, |g| {
            g.iterations[0].finished = true;
            g.iterations[0].outcome_status = Some(TurnStatus::Ok);
            g.phase = GoalPhase::Verifying;
            g.review_generation = 1;
            g.review_candidate = Some(candidate.clone());
            g.reviews.push(review);
            g.evidence_versions = vec![("proof.txt".into(), old_hash)];
            g.checker_records.push(GoalCheckerRecord {
                candidate: candidate.clone(),
                checker_id: "known-checker".into(),
                status: GoalCriterionStatus::Pass,
                reason: "known result".into(),
                evidence: vec!["proof.txt".into()],
            });
            g.checker_passed_candidate = Some(candidate);
            g.checker_evidence_versions = vec![("proof.txt".into(), Some(new_hash))];
            Ok(())
        })
        .unwrap();
    runtime.shutdown().await.unwrap();
    drop(handle);
    let backend = Arc::new(GoalReportBackend {
        calls: Default::default(),
        continue_first: false,
    });
    let resumed = AgentRuntime::start(
        AgentOptions::new(&home, &cwd)
            .with_session(SessionPolicy::Resume(sid))
            .with_llm_backend(backend.clone())
            .with_goal_verifier(verifier),
    )
    .await
    .unwrap();
    let h = resumed.handle();
    h.start_goal_driver_and_run().unwrap();
    let goal = h.wait_goal().await.unwrap();
    resumed.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    assert_eq!(goal.status, GoalStatus::Completed, "{:?}", goal.reason);
    assert_eq!(goal.completion_basis, Some(GoalCompletionBasis::Verified));
    assert_eq!(goal.iterations.len(), 1);
    assert_eq!(goal.reviews.len(), 2);
    assert_eq!(goal.checker_records.len(), 1);
    assert_eq!(backend.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(goal.pending_effects.is_empty());
}

#[tokio::test]
async fn goal_last_round_reaudits_checker_changes_without_reexecuting() {
    use crate::runtime::*;
    struct MutateOnce(std::sync::atomic::AtomicUsize);
    #[async_trait::async_trait]
    impl GoalVerifier for MutateOnce {
        fn id(&self) -> &str {
            "mutate-once"
        }
        async fn verify(
            &self,
            context: GoalVerificationContext,
        ) -> anyhow::Result<GoalVerification> {
            if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                std::fs::write(context.cwd.join("proof.txt"), "proof refreshed by checker")?;
            }
            Ok(GoalVerification {
                status: GoalVerificationStatus::Pass,
                reason: "checked refreshed proof".into(),
                evidence: vec!["proof.txt".into()],
            })
        }
    }
    let verifier = Arc::new(MutateOnce(Default::default()));
    let (runtime, home, cwd, backend) =
        goal_runtime("goal-reaudit-last", false, Some(verifier.clone())).await;
    let handle = runtime.handle();
    let mut spec = GoalSpec::new("produce proof");
    spec.limits.max_turns = 1;
    handle.confirmed_test_goal(spec).unwrap();
    handle.start_goal_driver_and_run().unwrap();
    let goal = handle.wait_goal().await.unwrap();
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    assert_eq!(goal.status, GoalStatus::Completed);
    assert_eq!(goal.completion_basis, Some(GoalCompletionBasis::Verified));
    assert_eq!(goal.iterations.len(), 1);
    assert_eq!(goal.reviews.len(), 2);
    assert_ne!(goal.reviews[0].candidate, goal.reviews[1].candidate);
    assert_eq!(verifier.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(goal.checker_records.len(), 1);
    assert_eq!(backend.calls.load(std::sync::atomic::Ordering::SeqCst), 5);
    assert_eq!(goal.usage.request_count, 5);
    assert!(goal.pending_effects.is_empty());
}

#[tokio::test]
async fn goal_verifier_pass_and_evidence_mutation_have_distinct_completion() {
    use crate::runtime::*;
    for mutate in [false, true] {
        let verifier = Arc::new(TestGoalVerifier {
            status: GoalVerificationStatus::Pass,
            mutate,
        });
        let (runtime, home, cwd, _) =
            goal_runtime("goal-proof-version", false, Some(verifier)).await;
        let handle = runtime.handle();
        let mut spec = GoalSpec::new("produce proof");
        spec.acceptance = GoalAcceptance::Verifier {
            id: "test-check-v1".into(),
        };
        spec.limits.max_turns = 2;
        handle.confirmed_test_goal(spec).unwrap();
        handle.start_goal_driver_and_run().unwrap();
        let goal = handle.wait_goal().await.unwrap();
        assert_eq!(goal.status, GoalStatus::Completed);
        assert_eq!(goal.completion_basis, Some(GoalCompletionBasis::Verified));
        assert_eq!(goal.iterations.len(), 1);
        assert_eq!(goal.reviews.len(), if mutate { 2 } else { 1 });
        assert_eq!(goal.checker_records.len(), 1);
        runtime.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(cwd);
    }
}

#[tokio::test]
async fn goal_human_approval_rejects_expired_deadline() {
    use crate::runtime::*;
    let (runtime, home, cwd, _) = goal_runtime("goal-human-deadline", false, None).await;
    let handle = runtime.handle();
    let mut spec = GoalSpec::new("produce proof");
    spec.acceptance = GoalAcceptance::Human;
    handle.confirmed_test_goal(spec).unwrap();
    handle.start_goal_driver_and_run().unwrap();
    let waiting = handle.wait_goal().await.unwrap();
    let mut limits = waiting.spec.limits.clone();
    limits.deadline_unix_secs = 1;
    let expired = handle
        .update_goal_limits(&waiting.id, waiting.revision, limits)
        .unwrap();
    let approved = handle.approve_goal(&expired.id, expired.revision, vec!["checked proof".into()]);
    let current = handle.goal_snapshot().unwrap();
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    assert!(
        approved.is_err(),
        "expired approval was accepted: {approved:?}"
    );
    assert_eq!(
        current, expired,
        "a rejected approval must not publish success"
    );
}

#[tokio::test]
async fn goal_human_wait_owns_evidence_and_approval_has_human_basis() {
    use crate::runtime::*;
    let (runtime, home, cwd, _) = goal_runtime("goal-human-pass", false, None).await;
    let handle = runtime.handle();
    let mut spec = GoalSpec::new("produce proof");
    spec.acceptance = GoalAcceptance::Human;
    handle.confirmed_test_goal(spec).unwrap();
    handle.start_goal_driver_and_run().unwrap();
    let goal = handle.wait_goal().await.unwrap();
    assert!(matches!(
        handle.stream_turn("change evidence"),
        Err(RuntimeError::Busy { .. })
    ));
    let completed = handle
        .approve_goal(&goal.id, goal.revision, vec!["reviewed proof".into()])
        .unwrap();
    assert_eq!(
        completed.completion_basis,
        Some(GoalCompletionBasis::HumanApproved)
    );
    assert!(
        handle
            .approve_goal(&goal.id, goal.revision, vec!["duplicate".into()])
            .is_err()
    );
    assert!(
        handle
            .resume_goal(&completed.id, completed.revision)
            .is_err()
    );
    handle
        .clear_goal(&completed.id, completed.revision)
        .unwrap();
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_empty_surface_rejects_creation_and_rust_start_does_not_schedule() {
    use crate::runtime::*;
    let home = unique_temp_dir("goal-empty-home");
    let cwd = unique_temp_dir("goal-empty-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let runtime = AgentRuntime::start(
        AgentOptions::new(&home, &cwd).with_enabled_tools(Vec::<String>::new()),
    )
    .await
    .unwrap();
    assert!(
        runtime
            .handle()
            .confirmed_test_goal(GoalSpec::new("work"))
            .is_err()
    );
    runtime.shutdown().await.unwrap();
    let (runtime, h, c, backend) = goal_runtime("goal-optin", false, None).await;
    runtime
        .handle()
        .confirmed_test_goal(GoalSpec::new("work"))
        .unwrap();
    tokio::task::yield_now().await;
    assert_eq!(backend.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    let _ = std::fs::remove_dir_all(h);
    let _ = std::fs::remove_dir_all(c);
}

#[tokio::test]
async fn goal_deadline_interrupts_pending_request_and_cancel_is_terminal() {
    use crate::runtime::*;
    let home = unique_temp_dir("goal-deadline-home");
    let cwd = unique_temp_dir("goal-deadline-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let backend = Arc::new(PendingEstablishBackend::default());
    let runtime =
        AgentRuntime::start(AgentOptions::new(&home, &cwd).with_llm_backend(backend.clone()))
            .await
            .unwrap();
    let handle = runtime.handle();
    let mut spec = GoalSpec::new("work");
    spec.limits.deadline_unix_secs = crate::session::goal::goal_now() + 3;
    handle.confirmed_test_goal(spec).unwrap();
    handle.start_goal_driver_and_run().unwrap();
    let goal = tokio::time::timeout(std::time::Duration::from_secs(10), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(goal.status, GoalStatus::BudgetExhausted);
    assert_eq!(*backend.calls.lock().unwrap(), 1);
    let cancelled = handle.cancel_goal(&goal.id, goal.revision).unwrap();
    assert_eq!(cancelled.status, GoalStatus::Cancelled);
    assert!(
        handle
            .resume_goal(&cancelled.id, cancelled.revision)
            .is_err()
    );
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

/// Host-side capacity stand-in: counts how many leases a run holds.
struct CountingLease(std::sync::Arc<std::sync::atomic::AtomicUsize>);
impl CountingLease {
    fn new(alive: &std::sync::Arc<std::sync::atomic::AtomicUsize>) -> Self {
        alive.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(alive.clone())
    }
}
impl crate::runtime::GoalRunLease for CountingLease {}
impl Drop for CountingLease {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[tokio::test]
async fn host_lease_lives_for_exactly_one_run() {
    use crate::runtime::*;
    let (runtime, home, cwd, _) = goal_runtime("goal-lease", false, None).await;
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    let alive = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let goal = handle
        .confirmed_test_goal_with_lease(
            GoalSpec::new("produce proof"),
            Box::new(CountingLease::new(&alive)),
        )
        .unwrap();
    assert_eq!(
        alive.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the lease must be held while the run owns scheduling"
    );
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome.id, goal.id);
    assert_eq!(outcome.status, GoalStatus::Completed);
    for _ in 0..200 {
        if alive.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        alive.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the lease must be released when the run ends"
    );
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn resumed_host_activation_carries_its_lease() {
    use crate::runtime::*;
    let (runtime, home, cwd, _) = goal_runtime("goal-lease-resume", false, None).await;
    let handle = runtime.handle();
    // Create and pause while the driver is still disabled, then enable it: the
    // resume is the activation that must own a fresh run and lease.
    let goal = handle
        .confirmed_test_goal(GoalSpec::new("produce proof"))
        .unwrap();
    let paused = handle.pause_goal(&goal.id, goal.revision).unwrap();
    handle.start_goal_driver_and_run().unwrap();
    let alive = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let resumed = handle
        .resume_and_start_goal(
            &paused.id,
            paused.revision,
            Box::new(CountingLease::new(&alive)),
        )
        .unwrap();
    assert_eq!(resumed.status, GoalStatus::Active);
    assert_eq!(alive.load(std::sync::atomic::Ordering::SeqCst), 1);
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome.status, GoalStatus::Completed);
    for _ in 0..200 {
        if alive.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(alive.load(std::sync::atomic::Ordering::SeqCst), 0);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn refused_host_activation_destroys_its_lease() {
    use crate::runtime::*;
    // Driver disabled: a host-managed activation must not leave an active goal
    // that nothing runs.
    let (runtime, home, cwd, _) = goal_runtime("goal-lease-refuse", false, None).await;
    let handle = runtime.handle();
    let alive = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let draft = handle
        .draft_goal(GoalSpec::new("not started"), 3600)
        .unwrap();
    let disabled = handle.confirm_and_start_goal(
        &draft.id,
        draft.revision,
        Box::new(CountingLease::new(&alive)),
    );
    assert!(matches!(disabled, Err(RuntimeError::Configuration(_))));
    assert_eq!(
        alive.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a refused activation must destroy its lease"
    );
    assert!(
        handle.goal_snapshot() == Some(draft.clone()),
        "a refused confirmation must leave the draft unchanged"
    );
    // A run already owning the driver refuses the activation before the
    // contract is touched, and still destroys the lease.
    handle.start_goal_driver_and_run().unwrap();
    {
        let mut control = handle.goal_driver.lock_control();
        control.driver = crate::runtime::goal::DriverState::Running {
            run_id: 1,
            goal_id: "g".into(),
        };
    }
    let busy = handle.confirm_and_start_goal(
        &draft.id,
        draft.revision,
        Box::new(CountingLease::new(&alive)),
    );
    assert!(matches!(busy, Err(RuntimeError::Busy { .. })));
    assert_eq!(alive.load(std::sync::atomic::Ordering::SeqCst), 0);
    // A latched session refuses the activation and destroys the lease too.
    {
        let mut control = handle.goal_driver.lock_control();
        control.driver = crate::runtime::goal::DriverState::Idle;
    }
    runtime.handle().goal_ctx.persistence_fault.raise(
        std::path::Path::new("goal.json"),
        "injected publish failure",
    );
    let latched = handle.confirmed_test_goal_with_lease(
        GoalSpec::new("latched"),
        Box::new(CountingLease::new(&alive)),
    );
    assert!(latched.is_err());
    assert_eq!(alive.load(std::sync::atomic::Ordering::SeqCst), 0);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goal_persistence_fault_stops_scheduling_without_restart() {
    use crate::runtime::*;
    let (runtime, home, cwd, backend) = goal_runtime("goal-fault-stop", true, None).await;
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    let goal = handle
        .confirmed_test_goal(GoalSpec::new("produce proof"))
        .unwrap();

    // Let the first iteration reach the provider, then latch the session as a
    // real publish failure would.
    for _ in 0..200 {
        if backend.calls.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        backend.calls.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "goal turn never started"
    );
    runtime.handle().goal_ctx.persistence_fault.raise(
        std::path::Path::new("goal.json"),
        "injected publish failure",
    );

    // An unrecoverable failure must stop scheduling: wait_goal returns the
    // runtime fault it could not persist instead of spinning on a disk state
    // that still reads `active`.
    let waited = tokio::time::timeout(std::time::Duration::from_secs(10), handle.wait_goal())
        .await
        .expect("driver must stop after an unrecoverable failure");
    assert!(
        waited.is_err(),
        "the runtime fault must surface: {waited:?}"
    );
    assert_eq!(
        handle
            .goal_driver
            .drive_attempts
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a failed run must not restart while the goal still reads active"
    );
    // A faulted driver refuses new activation instead of pretending to run.
    assert!(handle.start_goal_driver().is_err());
    assert!(
        handle
            .confirmed_test_goal(GoalSpec::new("second goal"))
            .is_err()
    );
    assert_eq!(
        handle
            .goal_snapshot()
            .map(|current| (current.id, current.status)),
        Some((goal.id.clone(), GoalStatus::Active)),
        "a fault that cannot be persisted must not fake a failed status"
    );

    let shutdown =
        tokio::time::timeout(std::time::Duration::from_secs(20), runtime.shutdown()).await;
    assert!(
        shutdown.is_ok(),
        "shutdown must stay bounded after the driver already stopped"
    );
    assert!(shutdown.unwrap().is_ok());
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

/// The same fault-stop scenario, but observed from outside the process: a
/// hot restart loop cannot hide behind the test executor's scheduling when
/// the child process itself is bounded. The child asserts the observable
/// attempt count; the parent enforces the wall-clock deadline and kills a
/// child that never stops.
#[test]
fn goal_fault_stop_is_bounded_in_a_child_process() {
    if std::env::var("MINK_GOAL_FAULT_CHILD").is_ok() {
        return; // the child runs `goal_fault_stop_child` instead
    }
    let exe = std::env::current_exe().expect("test binary");
    let mut child = std::process::Command::new(exe)
        .args([
            "--exact",
            "runtime::builder::tests::goal_fault_stop_child",
            "--nocapture",
        ])
        .env("MINK_GOAL_FAULT_CHILD", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the fault-stop child");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match child.try_wait().expect("poll the fault-stop child") {
            Some(status) => {
                let output = child
                    .wait_with_output()
                    .expect("collect the fault-stop child output");
                assert!(
                    status.success(),
                    "fault-stop child failed: {}\n{}{}",
                    status,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(
                    String::from_utf8_lossy(&output.stdout).contains("goal_fault_stop_child_ok"),
                    "the child never reported the scenario: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                return;
            }
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "fault-stop child never stopped; the driver is likely retrying in a hot loop"
                );
            }
            None => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
}

#[test]
fn goal_fault_stop_child() {
    if std::env::var("MINK_GOAL_FAULT_CHILD").is_err() {
        return; // parent-only discovery
    }
    use crate::runtime::*;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("child runtime");
    runtime.block_on(async {
        let (runtime, home, cwd, backend) = goal_runtime("goal-fault-child", true, None).await;
        let handle = runtime.handle();
        handle.start_goal_driver_and_run().unwrap();
        handle
            .confirmed_test_goal(GoalSpec::new("produce proof"))
            .unwrap();
        for _ in 0..200 {
            if backend.calls.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        runtime.handle().goal_ctx.persistence_fault.raise(
            std::path::Path::new("goal.json"),
            "injected publish failure",
        );
        let waited = tokio::time::timeout(std::time::Duration::from_secs(10), handle.wait_goal())
            .await
            .expect("the fault must stop the driver");
        assert!(waited.is_err(), "the runtime fault must surface");
        assert_eq!(
            handle
                .goal_driver
                .drive_attempts
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the child observed a driver restart"
        );
        runtime.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(cwd);
        println!("goal_fault_stop_child_ok");
    });
}

/// Sink that fails every event, producing a driver error without latching the
/// session's persistence fault.
struct FailingSink;

#[async_trait::async_trait]
impl EventSink for FailingSink {
    async fn on_event(&self, _event: AgentEvent) -> Result<(), String> {
        Err("injected sink failure".into())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goal_driver_fault_is_stable_and_blocks_reactivation() {
    use crate::runtime::*;
    let home = unique_temp_dir("goal-sink-fault-home");
    let cwd = unique_temp_dir("goal-sink-fault-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("proof.txt"), "objective evidence").unwrap();
    let backend = Arc::new(GoalReportBackend {
        calls: Default::default(),
        continue_first: false,
    });
    let runtime = AgentRuntime::start(
        AgentOptions::new(&home, &cwd)
            .with_llm_backend(backend.clone())
            .with_goal_event_sink(Arc::new(FailingSink)),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    let goal = handle
        .confirmed_test_goal(GoalSpec::new("produce proof"))
        .unwrap();

    let waited = tokio::time::timeout(std::time::Duration::from_secs(10), handle.wait_goal())
        .await
        .expect("a sink failure must stop the run");
    let error = waited.expect_err("the driver fault must surface");
    assert!(
        format!("{error}").contains("sink") || format!("{error}").contains("fault"),
        "the fault keeps its cause: {error}"
    );
    assert_eq!(
        handle
            .goal_driver
            .drive_attempts
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a faulted driver must not restart automatically"
    );
    // Whether the turn outcome was publishable or not, the runtime fault is
    // the stable terminal driver state...
    assert!(matches!(
        handle.goal_driver.state(),
        crate::runtime::goal::DriverState::Faulted { .. }
    ));
    // ...and reactivation is refused until the session is reopened, even for a
    // goal whose failed status made it replaceable.
    assert!(handle.start_goal_driver().is_err());
    assert!(
        handle
            .confirmed_test_goal(GoalSpec::new("replacement"))
            .is_err()
    );
    let _ = goal;
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn failed_stop_intent_stops_the_run_and_never_reads_as_saved() {
    use crate::runtime::*;
    let (runtime, home, cwd, _) = goal_runtime("goal-stop-intent", true, None).await;
    let handle = runtime.handle();
    // A goal exists without a real run; force the driver into Running so the
    // pause path believes it owns an in-flight run.
    let goal = handle
        .confirmed_test_goal(GoalSpec::new("produce proof"))
        .unwrap();
    {
        let mut control = handle.goal_driver.lock_control();
        control.enabled = true;
        control.driver = crate::runtime::goal::DriverState::Running {
            run_id: 7,
            goal_id: goal.id.clone(),
        };
    }
    runtime.handle().goal_ctx.persistence_fault.raise(
        std::path::Path::new("goal.json"),
        "injected publish failure",
    );

    let paused = handle.pause_goal(&goal.id, goal.revision);
    assert!(
        paused.is_err(),
        "a stop intent that cannot be saved must fail"
    );
    // The failed intent latches the run in `Stopping` with its reason; the
    // stable fault waits for the run's own settlement. This fake run has no
    // worker, so waiters are not woken yet: a fault must never be published
    // before the execution it interrupts has actually exited.
    assert!(
        matches!(
            handle.goal_driver.state(),
            crate::runtime::goal::DriverState::Stopping { run_id: 7, reason, .. }
                if reason.contains("stop intent could not be persisted")
        ),
        "the run must stop instead of continuing on an unpersisted stop intent: {:?}",
        handle.goal_driver.state()
    );
    let pending =
        tokio::time::timeout(std::time::Duration::from_millis(200), handle.wait_goal()).await;
    assert!(
        pending.is_err(),
        "the stable fault must not be published before execution exits"
    );
    // The run settlement publishes the stable fault once execution is gone
    // (what `run_goal_worker` does for a real run).
    handle.finish_goal_run(
        7,
        &goal.id,
        Some("goal stop intent could not be persisted".into()),
    );
    let waited = tokio::time::timeout(std::time::Duration::from_secs(5), handle.wait_goal())
        .await
        .expect("waiters must be woken by the stable fault");
    assert!(waited.is_err());
    // Reactivation is refused while the run is faulted.
    assert!(handle.start_goal_driver().is_err());
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn stale_run_finish_cannot_clear_a_newer_run() {
    let (runtime, home, cwd, _) = goal_runtime("goal-stale-run", false, None).await;
    let handle = runtime.handle();
    // A newer run owns the driver with run_id 2.
    {
        let mut control = handle.goal_driver.control.lock().unwrap();
        control.driver = crate::runtime::goal::DriverState::Running {
            run_id: 2,
            goal_id: "goal-b".into(),
        };
    }
    // A late cleanup for the older run must not touch the newer run.
    handle.finish_goal_run(1, "goal-a", None);
    assert!(matches!(
        handle.goal_driver.state(),
        crate::runtime::goal::DriverState::Running { run_id: 2, .. }
    ));
    handle.finish_goal_run(2, "goal-b", None);
    assert!(matches!(
        handle.goal_driver.state(),
        crate::runtime::goal::DriverState::Idle
    ));
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_settlement_is_idempotent_and_rejects_conflicting_facts() {
    use crate::runtime::*;
    use crate::session::goal::{ApplyResult, IterationKey, SettlementFacts};
    let (runtime, home, cwd, _) = goal_runtime("goal-settle", false, None).await;
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    handle
        .confirmed_test_goal(GoalSpec::new("produce proof"))
        .unwrap();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome.status, GoalStatus::Completed);
    let iteration = outcome.iterations.last().expect("settled iteration");
    let key = IterationKey {
        goal_id: outcome.id.clone(),
        iteration: iteration.iteration,
        turn_id: iteration.turn_id.clone(),
    };
    let result = crate::agent::orchestrator::TurnRunResult {
        billing_turn_id: iteration.billing_turn_id.clone().expect("billing id"),
        status: TurnStatus::Ok,
        tool_call_count: 0,
        tool_error_count: 0,
        error: None,
        usage_records: vec![],
        usage: Default::default(),
        text: String::new(),
        thinking: String::new(),
    };
    let before = handle.goal_snapshot().unwrap();
    let applied = handle
        .goal_ctx
        .goal_store
        .settle_iteration(&key, &result, &SettlementFacts::default())
        .unwrap();
    assert_eq!(applied, ApplyResult::AlreadyApplied);
    let after = handle.goal_snapshot().unwrap();
    assert_eq!(
        before.revision, after.revision,
        "an idempotent repeat must not bump the revision"
    );
    assert_eq!(
        before.usage.request_count, after.usage.request_count,
        "an idempotent repeat must not duplicate usage"
    );
    // Conflicting facts for the same identity are an error, not an overwrite.
    let conflicting = crate::agent::orchestrator::TurnRunResult {
        billing_turn_id: "turn-someone-else".into(),
        ..result.clone()
    };
    assert!(
        handle
            .goal_ctx
            .goal_store
            .settle_iteration(&key, &conflicting, &SettlementFacts::default())
            .is_err(),
        "conflicting facts must not silently overwrite a settled iteration"
    );
    // An unknown identity is refused instead of settling whatever is current.
    let unknown = IterationKey {
        goal_id: outcome.id.clone(),
        iteration: iteration.iteration,
        turn_id: "turn-that-never-ran".into(),
    };
    assert!(
        handle
            .goal_ctx
            .goal_store
            .settle_iteration(&unknown, &result, &SettlementFacts::default())
            .is_err()
    );
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn stale_settlement_fills_history_without_touching_the_replacement() {
    use crate::runtime::*;
    use crate::session::goal::{IterationKey, SettlementFacts};
    let (runtime, home, cwd, _) = goal_runtime("goal-settle-history", false, None).await;
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    handle
        .confirmed_test_goal(GoalSpec::new("first goal"))
        .unwrap();
    let first = tokio::time::timeout(std::time::Duration::from_secs(30), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    let iteration = first.iterations.last().expect("settled iteration");
    let key = IterationKey {
        goal_id: first.id.clone(),
        iteration: iteration.iteration,
        turn_id: iteration.turn_id.clone(),
    };
    let result = crate::agent::orchestrator::TurnRunResult {
        billing_turn_id: iteration.billing_turn_id.clone().expect("billing id"),
        status: TurnStatus::Ok,
        tool_call_count: 0,
        tool_error_count: 0,
        error: None,
        usage_records: vec![],
        usage: Default::default(),
        text: String::new(),
        thinking: String::new(),
    };
    // Stop the driver before creating the replacement, so the assertions below
    // describe a stable current goal.
    handle.goal_driver.disable();
    let replacement = handle
        .confirmed_test_goal(GoalSpec::new("replacement goal"))
        .unwrap();
    let before = handle.goal_snapshot().unwrap();
    assert_eq!(before.id, replacement.id);
    // The archived goal's round settles in history; the replacement is untouched.
    handle
        .goal_ctx
        .goal_store
        .settle_iteration(&key, &result, &SettlementFacts::default())
        .expect("archive settlement");
    let after = handle.goal_snapshot().unwrap();
    assert_eq!(after.id, replacement.id);
    assert_eq!(after.revision, before.revision);
    assert!(
        after.usage_records.is_empty(),
        "the replacement must not absorb the old goal's usage"
    );
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn identity_bound_updates_never_touch_a_replacement() {
    use crate::runtime::*;
    let (runtime, home, cwd, _) = goal_runtime("goal-update-identity", false, None).await;
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    handle
        .confirmed_test_goal(GoalSpec::new("first goal"))
        .unwrap();
    let first = tokio::time::timeout(std::time::Duration::from_secs(30), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.status, GoalStatus::Completed);
    // Stop the driver before creating the replacement so the current goal is
    // stable while the stale update is attempted.
    handle.goal_driver.disable();
    let replacement = handle
        .confirmed_test_goal(GoalSpec::new("replacement goal"))
        .unwrap();
    // A decision that belongs to the finished goal must not reach the
    // replacement, which is what every driver write now relies on.
    let applied = handle
        .goal_ctx
        .goal_store
        .update_if_current(&first.id, |g| {
            g.status = GoalStatus::Failed;
            g.reason = Some("stale decision".into());
            Ok(())
        })
        .unwrap();
    assert!(applied.is_none(), "a stale update must apply to nothing");
    let current = handle.goal_snapshot().unwrap();
    assert_eq!(current.id, replacement.id);
    assert_eq!(current.status, GoalStatus::Active);
    assert_eq!(current.reason, replacement.reason);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_restore_active_requires_explicit_rust_driver_and_paused_stays_paused() {
    use crate::runtime::*;
    for paused in [false, true] {
        let (runtime, home, cwd, _) = goal_runtime("goal-restore", false, None).await;
        let sid = runtime.session_info().session_id.clone();
        let handle = runtime.handle();
        let goal = handle
            .confirmed_test_goal(GoalSpec::new("produce proof"))
            .unwrap();
        if paused {
            handle.pause_goal(&goal.id, goal.revision).unwrap();
        }
        runtime.shutdown().await.unwrap();
        let backend = Arc::new(GoalReportBackend {
            calls: Default::default(),
            continue_first: false,
        });
        let runtime = AgentRuntime::start(
            AgentOptions::new(&home, &cwd)
                .with_session(SessionPolicy::Resume(sid))
                .with_llm_backend(backend.clone()),
        )
        .await
        .unwrap();
        let handle = runtime.handle();
        assert_eq!(backend.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            handle.goal_snapshot().unwrap().status,
            if paused {
                GoalStatus::Paused
            } else {
                GoalStatus::Active
            }
        );
        // Arming is not starting: a restored active goal must not run until
        // an explicit lifecycle call picks it up.
        handle.start_goal_driver().unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            backend.calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "start_goal_driver must not start a run"
        );
        assert!(!handle.goal_driver.is_active());
        handle.start_goal_driver_and_run().unwrap();
        let goal = handle.wait_goal().await.unwrap();
        assert_eq!(
            goal.status,
            if paused {
                GoalStatus::Paused
            } else {
                GoalStatus::Completed
            }
        );
        assert_eq!(
            backend.calls.load(std::sync::atomic::Ordering::SeqCst),
            if paused { 0 } else { 3 }
        );
        runtime.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(cwd);
    }
}

/// Audit8 F1: a restored `active` goal is started by the leased activation,
/// and by nothing else. Arming first must issue no provider request and hold
/// no run; `resume_and_start_goal` is then the single owner of the run and of
/// the host lease it carries.
#[tokio::test]
async fn audit8_restored_active_goal_starts_only_through_a_leased_resume() {
    use crate::runtime::*;
    // Create while scheduling is disabled, then close the runtime: the disk
    // keeps the contract as `active` with no run behind it.
    let (runtime, home, cwd, backend) = goal_runtime("audit8-resume", false, None).await;
    let sid = runtime.session_info().session_id.clone();
    let handle = runtime.handle();
    let goal = handle
        .confirmed_test_goal(GoalSpec::new("produce proof"))
        .unwrap();
    assert_eq!(backend.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    runtime.shutdown().await.unwrap();

    let backend = Arc::new(GoalReportBackend {
        calls: Default::default(),
        continue_first: false,
    });
    let runtime = AgentRuntime::start(
        AgentOptions::new(&home, &cwd)
            .with_session(SessionPolicy::Resume(sid))
            .with_llm_backend(backend.clone()),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let restored = handle.goal_snapshot().unwrap();
    assert_eq!(restored.status, GoalStatus::Active);
    handle.start_goal_driver().unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        backend.calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "reopen + arm must not issue a provider request"
    );
    assert!(!handle.goal_driver.is_active());

    let alive = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let resumed = handle
        .resume_and_start_goal(
            &goal.id,
            restored.revision,
            Box::new(CountingLease::new(&alive)),
        )
        .unwrap();
    assert_eq!(resumed.status, GoalStatus::Active);
    assert_eq!(alive.load(std::sync::atomic::Ordering::SeqCst), 1);
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome.status, GoalStatus::Completed);
    for _ in 0..200 {
        if alive.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        alive.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the resumed run's lease is released when the run ends"
    );
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

struct HeldGoalVerifier {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl crate::runtime::GoalVerifier for HeldGoalVerifier {
    fn id(&self) -> &str {
        "held-check-v1"
    }
    async fn verify(
        &self,
        _context: crate::runtime::GoalVerificationContext,
    ) -> anyhow::Result<crate::runtime::GoalVerification> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(crate::runtime::GoalVerification {
            status: crate::runtime::GoalVerificationStatus::Pass,
            reason: "released".into(),
            evidence: vec![],
        })
    }
}
#[tokio::test]
async fn goal_pause_during_verifier_keeps_execution_owned_until_checker_exits() {
    use crate::runtime::*;
    let verifier = Arc::new(HeldGoalVerifier {
        entered: Default::default(),
        release: Default::default(),
    });
    let (runtime, home, cwd, _) =
        goal_runtime("goal-held-check", false, Some(verifier.clone())).await;
    let handle = runtime.handle();
    let mut spec = GoalSpec::new("produce proof");
    spec.acceptance = GoalAcceptance::Verifier {
        id: "held-check-v1".into(),
    };
    handle.confirmed_test_goal(spec).unwrap();
    handle.start_goal_driver_and_run().unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        verifier.entered.notified(),
    )
    .await
    .unwrap();
    let g = handle.goal_snapshot().unwrap();
    handle.pause_goal(&g.id, g.revision).unwrap();
    let paused = handle.wait_goal().await.unwrap();
    assert_eq!(paused.status, GoalStatus::Paused);
    assert_eq!(paused.completion_basis, None);
    assert!(
        matches!(
            handle.stream_turn("independent work"),
            Err(RuntimeError::Busy { .. })
        ),
        "checker still owns permit"
    );
    assert!(handle.resume_goal(&paused.id, paused.revision).is_err());
    verifier.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while handle.active_turn_id().is_some() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let g = handle.goal_snapshot().unwrap();
    handle.cancel_goal(&g.id, g.revision).unwrap();
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

/// Audit8 F2: a paused run settles immediately, but the host lease must
/// outlive the checker that is still exiting with the turn gate. The lease is
/// dropped only after the gate goes idle, and no caller blocks meanwhile.
#[tokio::test]
async fn audit8_paused_checker_keeps_the_host_lease_until_it_exits() {
    use crate::runtime::*;
    let verifier = Arc::new(HeldGoalVerifier {
        entered: Default::default(),
        release: Default::default(),
    });
    let (runtime, home, cwd, _) =
        goal_runtime("audit8-lease-held", false, Some(verifier.clone())).await;
    let handle = runtime.handle();
    handle.start_goal_driver().unwrap();
    let alive = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut spec = GoalSpec::new("produce proof");
    spec.acceptance = GoalAcceptance::Verifier {
        id: "held-check-v1".into(),
    };
    handle
        .confirmed_test_goal_with_lease(spec, Box::new(CountingLease::new(&alive)))
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        verifier.entered.notified(),
    )
    .await
    .unwrap();
    assert_eq!(alive.load(std::sync::atomic::Ordering::SeqCst), 1);
    let g = handle.goal_snapshot().unwrap();
    handle.pause_goal(&g.id, g.revision).unwrap();
    // The pause settles the run and `wait_goal` returns while the checker is
    // still exiting: the lease must not be released by that return.
    let paused = handle.wait_goal().await.unwrap();
    assert_eq!(paused.status, GoalStatus::Paused);
    assert!(
        handle.active_turn_id().is_some(),
        "the checker still owns the turn gate"
    );
    assert_eq!(
        alive.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "host capacity must stay occupied while the checker exits"
    );
    verifier.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while handle.active_turn_id().is_some()
            || alive.load(std::sync::atomic::Ordering::SeqCst) > 0
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the lease must be released once the checker exits");
    assert_eq!(alive.load(std::sync::atomic::Ordering::SeqCst), 0);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

/// Audit8 F1 (re-audit): a pause whose intent cannot be *published* (a plain
/// pre-publication failure, no global latch) must still stop the run, keep
/// execution ownership until the checker exits, and never let the released
/// checker commit an accepted completion.
#[cfg(unix)]
#[tokio::test]
async fn audit8_prepublication_stop_failure_stops_the_run() {
    use crate::runtime::*;
    use std::os::unix::fs::PermissionsExt;
    let verifier = Arc::new(HeldGoalVerifier {
        entered: Default::default(),
        release: Default::default(),
    });
    let (runtime, home, cwd, _) =
        goal_runtime("audit8-stop-readonly", false, Some(verifier.clone())).await;
    let session_dir = runtime
        .session_info()
        .conversation_path
        .parent()
        .unwrap()
        .to_path_buf();
    let handle = runtime.handle();
    handle.start_goal_driver().unwrap();
    let mut spec = GoalSpec::new("produce proof");
    spec.acceptance = GoalAcceptance::Verifier {
        id: "held-check-v1".into(),
    };
    handle.confirmed_test_goal(spec).unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        verifier.entered.notified(),
    )
    .await
    .unwrap();
    // The revision moved while the round settled: the stop command must carry
    // the live revision, or a stale-revision rejection (not a publication
    // failure) would be the thing under test.
    let goal = handle.goal_snapshot().unwrap();
    // A publication failure that does not latch the session: the stop intent
    // cannot be stored, but the session is not globally faulted.
    let original = std::fs::metadata(&session_dir).unwrap().permissions();
    std::fs::set_permissions(&session_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let stopped = handle.pause_goal(&goal.id, goal.revision);
    std::fs::set_permissions(&session_dir, original).unwrap();
    assert!(stopped.is_err(), "{stopped:?}");
    assert!(
        handle.goal_ctx.persistence_fault.check().is_ok(),
        "the failure is a plain publication failure, not a latched fault"
    );
    assert!(
        matches!(
            handle.goal_driver.state(),
            crate::runtime::goal::DriverState::Stopping { reason, .. }
                if reason.contains("stop intent could not be persisted")
        ),
        "the run must be latched for stopping: {:?}",
        handle.goal_driver.state()
    );
    // The checker still owns the turn gate: the fault is not stable yet.
    let early =
        tokio::time::timeout(std::time::Duration::from_millis(200), handle.wait_goal()).await;
    assert!(
        early.is_err(),
        "the fault must wait for the checker to exit"
    );
    verifier.release.notify_one();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), handle.wait_goal())
        .await
        .expect("the settlement must publish the stable fault");
    assert!(outcome.is_err(), "{outcome:?}");
    assert!(
        matches!(
            handle.goal_driver.state(),
            crate::runtime::goal::DriverState::Faulted { .. }
        ),
        "the refused pause must leave a stable fault: {:?}",
        handle.goal_driver.state()
    );
    let current = handle.goal_snapshot().unwrap();
    assert_eq!(
        current.status,
        GoalStatus::Active,
        "a refused pause must not be saved"
    );
    assert_eq!(
        current.completion_basis, None,
        "the released checker must not commit an acceptance after the refused stop"
    );
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

/// Audit8 F2 (re-audit): a host activation that cannot start a run must be
/// refused *before* any state is written, never swallowed after the contract
/// is already durable.
#[tokio::test]
async fn audit8_activation_without_executor_leaves_no_contract() {
    use crate::runtime::*;
    let (runtime, home, cwd, _) = goal_runtime("audit8-no-executor", false, None).await;
    let handle = runtime.handle();
    handle.start_goal_driver().unwrap();
    let alive = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let draft = handle
        .draft_goal(GoalSpec::new("cannot run"), 3600)
        .unwrap();
    let thread_draft = draft.clone();
    let thread_handle = handle.clone();
    let thread_alive = alive.clone();
    let leased = std::thread::spawn(move || {
        thread_handle.confirm_and_start_goal(
            &thread_draft.id,
            thread_draft.revision,
            Box::new(CountingLease::new(&thread_alive)),
        )
    })
    .join()
    .unwrap();
    let error = leased.expect_err("activation without an executor must be refused");
    assert!(
        format!("{error}").contains("async runtime"),
        "the refusal names the missing executor: {error}"
    );
    assert!(
        handle.goal_snapshot() == Some(draft.clone()),
        "the draft must not change when activation has no executor"
    );
    assert_eq!(alive.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(
        matches!(
            handle.goal_driver.state(),
            crate::runtime::goal::DriverState::Idle
        ),
        "a refused activation must not latch a fault"
    );
    // Lifecycle confirmation also leaves the existing draft unchanged.
    let thread_draft = draft.clone();
    let thread_handle = handle.clone();
    let created = std::thread::spawn(move || {
        thread_handle.confirm_goal(&thread_draft.id, thread_draft.revision)
    })
    .join()
    .unwrap();
    assert!(created.is_err(), "{created:?}");
    assert_eq!(handle.goal_snapshot(), Some(draft));
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

/// Audit8 F3 (re-audit): a failed stop intent keeps the run in `Stopping`
/// until the execution it interrupts really exits; only then is the fault
/// stable, and the reason survives the settlement.
#[tokio::test]
async fn audit8_failed_stop_waits_for_gate_before_stable_fault() {
    use crate::runtime::*;
    let verifier = Arc::new(HeldGoalVerifier {
        entered: Default::default(),
        release: Default::default(),
    });
    let (runtime, home, cwd, _) =
        goal_runtime("audit8-stop-latch", false, Some(verifier.clone())).await;
    let handle = runtime.handle();
    handle.start_goal_driver().unwrap();
    let alive = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut spec = GoalSpec::new("produce proof");
    spec.acceptance = GoalAcceptance::Verifier {
        id: "held-check-v1".into(),
    };
    handle
        .confirmed_test_goal_with_lease(spec, Box::new(CountingLease::new(&alive)))
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        verifier.entered.notified(),
    )
    .await
    .unwrap();
    let goal = handle.goal_snapshot().unwrap();
    runtime.handle().goal_ctx.persistence_fault.raise(
        std::path::Path::new("goal.json"),
        "audit injected publish failure",
    );
    assert!(handle.pause_goal(&goal.id, goal.revision).is_err());
    assert!(
        matches!(
            handle.goal_driver.state(),
            crate::runtime::goal::DriverState::Stopping { .. }
        ),
        "the fault must not be stable while the checker exits: {:?}",
        handle.goal_driver.state()
    );
    assert!(handle.active_turn_id().is_some());
    let early =
        tokio::time::timeout(std::time::Duration::from_millis(200), handle.wait_goal()).await;
    assert!(
        early.is_err(),
        "a fault is stable only after execution exits"
    );
    verifier.release.notify_one();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), handle.wait_goal())
        .await
        .expect("waiters must be woken by the stable fault");
    let error = outcome.expect_err("the failed stop intent must surface as a fault");
    assert!(
        format!("{error}").contains("stop intent could not be persisted"),
        "the stop reason survives: {error}"
    );
    assert!(
        matches!(
            handle.goal_driver.state(),
            crate::runtime::goal::DriverState::Faulted { .. }
        ),
        "{:?}",
        handle.goal_driver.state()
    );
    for _ in 0..200 {
        if alive.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        alive.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the lease is released once the checker has exited"
    );
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

/// A stale revision is a rejected command, not a failed stop intent: it must
/// not latch or interrupt the live run.
#[tokio::test]
async fn audit8_stale_stop_revision_does_not_stop_the_run() {
    use crate::runtime::*;
    let verifier = Arc::new(HeldGoalVerifier {
        entered: Default::default(),
        release: Default::default(),
    });
    let (runtime, home, cwd, _) =
        goal_runtime("audit8-stale-stop", false, Some(verifier.clone())).await;
    let handle = runtime.handle();
    handle.start_goal_driver().unwrap();
    let alive = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut spec = GoalSpec::new("produce proof");
    spec.acceptance = GoalAcceptance::Verifier {
        id: "held-check-v1".into(),
    };
    handle
        .confirmed_test_goal_with_lease(spec, Box::new(CountingLease::new(&alive)))
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        verifier.entered.notified(),
    )
    .await
    .unwrap();
    let goal = handle.goal_snapshot().unwrap();
    let stale = handle.pause_goal(&goal.id, goal.revision + 1);
    assert!(stale.is_err(), "{stale:?}");
    assert!(
        matches!(
            handle.goal_driver.state(),
            crate::runtime::goal::DriverState::Running { .. }
        ),
        "a stale command must not latch the run: {:?}",
        handle.goal_driver.state()
    );
    assert!(handle.active_turn_id().is_some());
    // The run is still stoppable with the live revision.
    let cancelled = handle.cancel_goal(&goal.id, goal.revision).unwrap();
    assert_eq!(cancelled.status, GoalStatus::Cancelled);
    verifier.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while handle.active_turn_id().is_some()
            || alive.load(std::sync::atomic::Ordering::SeqCst) > 0
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the cancelled run must exit and release its lease");
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_budget_change_updates_the_inflight_absolute_deadline() {
    use crate::runtime::*;
    let home = unique_temp_dir("goal-budget-live-home");
    let cwd = unique_temp_dir("goal-budget-live-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let backend = Arc::new(PendingEstablishBackend::default());
    let runtime =
        AgentRuntime::start(AgentOptions::new(&home, &cwd).with_llm_backend(backend.clone()))
            .await
            .unwrap();
    let handle = runtime.handle();
    handle.confirmed_test_goal(GoalSpec::new("work")).unwrap();
    handle.start_goal_driver_and_run().unwrap();
    backend.entered.notified().await;
    let goal = handle.goal_snapshot().unwrap();
    let mut limits = goal.spec.limits.clone();
    limits.deadline_unix_secs = crate::session::goal::goal_now() + 1;
    handle
        .update_goal_limits(&goal.id, goal.revision, limits)
        .unwrap();
    let exhausted = tokio::time::timeout(std::time::Duration::from_secs(5), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(exhausted.status, GoalStatus::BudgetExhausted);
    assert_eq!(*backend.calls.lock().unwrap(), 1);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_checker_deadline_cannot_release_ownership_or_complete() {
    use crate::runtime::*;
    let verifier = Arc::new(HeldGoalVerifier {
        entered: Default::default(),
        release: Default::default(),
    });
    let (runtime, home, cwd, _) =
        goal_runtime("goal-check-deadline", false, Some(verifier.clone())).await;
    let handle = runtime.handle();
    let mut spec = GoalSpec::new("produce proof");
    spec.acceptance = GoalAcceptance::Verifier {
        id: "held-check-v1".into(),
    };
    spec.limits.deadline_unix_secs = crate::session::goal::goal_now() + 4;
    handle.confirmed_test_goal(spec).unwrap();
    handle.start_goal_driver_and_run().unwrap();
    verifier.entered.notified().await;
    let exhausted = tokio::time::timeout(std::time::Duration::from_secs(8), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(exhausted.status, GoalStatus::BudgetExhausted);
    assert_eq!(exhausted.completion_basis, None);
    assert!(matches!(
        handle.stream_turn("independent"),
        Err(RuntimeError::Busy { .. })
    ));
    verifier.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while handle.active_turn_id().is_some() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn subagent_executor_retains_child_and_parent_session_leases() {
    let home = unique_temp_dir("lease-child-home");
    let cwd = unique_temp_dir("lease-child-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let runtime = AgentRuntime::start(AgentOptions::new(&home, &cwd))
        .await
        .unwrap();
    let parent_dir = runtime
        .session_info()
        .conversation_path
        .parent()
        .unwrap()
        .to_path_buf();
    let child = crate::agent::sub_executor::SubAgentExecutor::new(
        runtime.ctx.clone(),
        "sub-lease-test".into(),
        false,
        runtime.ctx.config.clone(),
    )
    .await
    .unwrap();
    // Child context is private to its executor: keeping the executor alive also
    // keeps any child tools' lease chain alive after parent teardown.
    runtime.shutdown().await.unwrap();
    assert!(crate::runtime::SessionLease::acquire_directory(&parent_dir).is_err());
    assert!(
        crate::runtime::SessionLease::acquire_directory(
            &parent_dir.join("subagents/sub-lease-test")
        )
        .is_err()
    );
    drop(child);
    // Dropping the executor releases the child's chain; the release can land on
    // another task, so wait for it with a short deadline instead of racing it
    // (a workspace-wide run schedules enough work to lose that race).
    let release_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match crate::runtime::SessionLease::acquire_directory(&parent_dir) {
            Ok(_lease) => break,
            Err(_) if std::time::Instant::now() < release_deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Err(error) => panic!("parent lease still held after the executor dropped: {error}"),
        }
    }
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_limit_change_during_verification_preserves_valid_acceptance() {
    use crate::runtime::*;
    let verifier = Arc::new(HeldGoalVerifier {
        entered: Default::default(),
        release: Default::default(),
    });
    let (runtime, home, cwd, _) =
        goal_runtime("goal-check-budget", false, Some(verifier.clone())).await;
    let handle = runtime.handle();
    let mut spec = GoalSpec::new("produce proof");
    spec.acceptance = GoalAcceptance::Verifier {
        id: "held-check-v1".into(),
    };
    handle.confirmed_test_goal(spec).unwrap();
    handle.start_goal_driver_and_run().unwrap();
    verifier.entered.notified().await;
    let goal = handle.goal_snapshot().unwrap();
    let mut limits = goal.spec.limits.clone();
    limits.max_turns = 30;
    handle
        .update_goal_limits(&goal.id, goal.revision, limits)
        .unwrap();
    verifier.release.notify_one();
    let completed = tokio::time::timeout(std::time::Duration::from_secs(5), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(completed.status, GoalStatus::Completed);
    assert_eq!(
        completed.completion_basis,
        Some(GoalCompletionBasis::Verified)
    );
    assert_eq!(completed.spec.limits.max_turns, 30);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

// ── 第三轮审计回归（源自 goal-audit-2026-10-09/round-3）──

#[tokio::test]
async fn audit3_verifier_obeys_shortened_deadline() {
    use crate::runtime::*;
    let verifier = Arc::new(HeldGoalVerifier {
        entered: Default::default(),
        release: Default::default(),
    });
    let (runtime, home, cwd, _) =
        goal_runtime("audit3-deadline", false, Some(verifier.clone())).await;
    let handle = runtime.handle();
    let mut spec = GoalSpec::new("produce proof");
    spec.acceptance = GoalAcceptance::Verifier {
        id: "held-check-v1".into(),
    };
    handle.confirmed_test_goal(spec).unwrap();
    handle.start_goal_driver_and_run().unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        verifier.entered.notified(),
    )
    .await
    .unwrap();
    let g = handle.goal_snapshot().unwrap();
    let mut limits = g.spec.limits.clone();
    limits.deadline_unix_secs = crate::session::goal::goal_now() + 1;
    handle
        .update_goal_limits(&g.id, g.revision, limits)
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let observed = handle.goal_snapshot().unwrap().status;
    verifier.release.notify_one();
    handle.wait_goal().await.unwrap();
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    assert_eq!(
        observed,
        GoalStatus::BudgetExhausted,
        "shortened deadline must apply during verification"
    );
}

struct Audit3FailingSink;
#[async_trait::async_trait]
impl crate::runtime::EventSink for Audit3FailingSink {
    async fn on_event(&self, _: crate::runtime::AgentEvent) -> Result<(), String> {
        Err("injected sink error".into())
    }
}
#[tokio::test]
async fn audit3_failed_sink_resets_turn_activity() {
    use crate::runtime::*;
    let home = unique_temp_dir("audit3-sink-home");
    let cwd = unique_temp_dir("audit3-sink-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let backend = Arc::new(PendingEstablishBackend::default());
    let runtime = AgentRuntime::start(
        AgentOptions::new(&home, &cwd)
            .with_llm_backend(backend)
            .with_goal_event_sink(Arc::new(Audit3FailingSink)),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let activity = handle.subscribe_goal_turns();
    handle
        .confirmed_test_goal(GoalSpec::new("test sink"))
        .unwrap();
    handle.start_goal_driver_and_run().unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle.wait_goal())
        .await
        .unwrap();
    let goal = handle.goal_snapshot().unwrap();
    let active = *activity.borrow();
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    // The sink failure faults the driver, and the failed status it managed to
    // publish stays authoritative.
    assert!(
        result.is_err(),
        "a sink failure must surface as a runtime fault: {result:?}"
    );
    assert_eq!(goal.status, GoalStatus::Failed);
    assert!(!active, "failed goal left turn_active=true");
}

#[tokio::test]
async fn audit3_human_approval_requires_nonblank_evidence() {
    use crate::runtime::*;
    let (runtime, home, cwd, _) = goal_runtime("audit3-blank", false, None).await;
    let handle = runtime.handle();
    let mut spec = GoalSpec::new("produce proof");
    spec.acceptance = GoalAcceptance::Human;
    handle.confirmed_test_goal(spec).unwrap();
    handle.start_goal_driver_and_run().unwrap();
    let g = handle.wait_goal().await.unwrap();
    let approved = handle.approve_goal(&g.id, g.revision, vec![String::new()]);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    assert!(
        approved.is_err(),
        "blank approval evidence was accepted: {approved:?}"
    );
}

struct Audit3HoldFinalSink {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    held: std::sync::atomic::AtomicBool,
}
#[async_trait::async_trait]
impl crate::runtime::EventSink for Audit3HoldFinalSink {
    async fn on_event(&self, event: crate::runtime::AgentEvent) -> Result<(), String> {
        if serde_json::to_value(event).unwrap()["type"] == "final"
            && !self.held.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(())
    }
}
#[tokio::test]
async fn audit3_old_iteration_cannot_bill_replacement_goal() {
    use crate::runtime::*;
    let home = unique_temp_dir("audit3-replace-home");
    let cwd = unique_temp_dir("audit3-replace-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("proof.txt"), "proof").unwrap();
    let sink = Arc::new(Audit3HoldFinalSink {
        entered: Default::default(),
        release: Default::default(),
        held: Default::default(),
    });
    let backend = Arc::new(GoalReportBackend {
        calls: Default::default(),
        continue_first: false,
    });
    let runtime = AgentRuntime::start(
        AgentOptions::new(&home, &cwd)
            .with_llm_backend(backend)
            .with_goal_event_sink(sink.clone()),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    handle
        .confirmed_test_goal(GoalSpec::new("old goal"))
        .unwrap();
    handle.start_goal_driver_and_run().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), sink.entered.notified())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while handle.active_turn_id().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let old = handle.goal_snapshot().unwrap();
    let old_bill = old.iterations[0].billing_turn_id.clone().unwrap();
    handle.cancel_goal(&old.id, old.revision).unwrap();
    assert!(matches!(
        handle.draft_goal(GoalSpec::new("new goal"), 3600),
        Err(RuntimeError::Busy { .. })
    ));
    // Inject a replacement to exercise stale callbacks even though the public
    // protocol refuses replacement until the previous run has exited.
    let new = handle
        .goal_ctx
        .goal_store
        .create(
            GoalSpec::new("new goal"),
            crate::runtime::goal::goal_environment(&cwd, &handle.goal_ctx.tool_surface),
        )
        .unwrap();
    sink.release.notify_one();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    let contaminated = outcome
        .usage_records
        .iter()
        .any(|r| r.billing_turn_id == old_bill);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    assert_eq!(outcome.id, new.id);
    assert!(
        !contaminated,
        "replacement inherited prior goal billing; repair_used={}, iterations={}",
        outcome.report_repair_used,
        outcome.iterations.len()
    );
}

#[tokio::test]
async fn audit3_recover_normal_end_after_idle_publication() {
    use crate::runtime::*;
    let (runtime, home, cwd, _) = goal_runtime("audit3-recovery", false, None).await;
    let handle = runtime.handle();
    let mut spec = GoalSpec::new("produce proof");
    spec.limits.max_turns = 1;
    handle.confirmed_test_goal(spec).unwrap();
    let sid = handle.session_info().session_id.clone();
    // Exact persisted boundary in drive_goal: result committed, phase=Idle,
    // normal completion must recover into independent verification.
    runtime
        .ctx
        .goal_store
        .update(|s| {
            s.phase = GoalPhase::Idle;
            s.iterations.push(crate::session::goal::GoalIteration {
                iteration: 1,
                turn_id: "crash-boundary".into(),
                billing_turn_id: None,
                finished: true,
                conversation_start: 0,
                outcome_status: Some(TurnStatus::Ok),
                outcome_error: None,
            });
            Ok(())
        })
        .unwrap();
    runtime.shutdown().await.unwrap();
    drop(handle);
    let backend = Arc::new(GoalReportBackend {
        calls: Default::default(),
        continue_first: false,
    });
    let resumed = AgentRuntime::start(
        AgentOptions::new(&home, &cwd)
            .with_session(SessionPolicy::Resume(sid))
            .with_llm_backend(backend),
    )
    .await
    .unwrap();
    let h = resumed.handle();
    h.start_goal_driver_and_run().unwrap();
    let result = h.wait_goal().await.unwrap();
    resumed.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    assert_eq!(
        result.status,
        GoalStatus::Completed,
        "recovery lost pending completion report: {:?}",
        result.reason
    );
}

// ── 第四轮审计回归（源自 goal-audit-2026-10-09/round-4）──
struct Audit4HoldFinalErrorSink {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    held: std::sync::atomic::AtomicBool,
}
#[async_trait::async_trait]
impl crate::runtime::EventSink for Audit4HoldFinalErrorSink {
    async fn on_event(&self, event: crate::runtime::AgentEvent) -> Result<(), String> {
        if serde_json::to_value(event).unwrap()["type"] == "final"
            && !self.held.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.entered.notify_one();
            self.release.notified().await;
            return Err("old sink failed".into());
        }
        Ok(())
    }
}
#[tokio::test]
async fn audit4_old_sink_error_cannot_fail_replacement() {
    use crate::runtime::*;
    let home = unique_temp_dir("audit3-replace-home");
    let cwd = unique_temp_dir("audit3-replace-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("proof.txt"), "proof").unwrap();
    let sink = Arc::new(Audit4HoldFinalErrorSink {
        entered: Default::default(),
        release: Default::default(),
        held: Default::default(),
    });
    let backend = Arc::new(GoalReportBackend {
        calls: Default::default(),
        continue_first: false,
    });
    let runtime = AgentRuntime::start(
        AgentOptions::new(&home, &cwd)
            .with_llm_backend(backend)
            .with_goal_event_sink(sink.clone()),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    handle
        .confirmed_test_goal(GoalSpec::new("old goal"))
        .unwrap();
    handle.start_goal_driver_and_run().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), sink.entered.notified())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while handle.active_turn_id().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let old = handle.goal_snapshot().unwrap();
    let old_bill = old.iterations[0].billing_turn_id.clone().unwrap();
    handle.cancel_goal(&old.id, old.revision).unwrap();
    assert!(matches!(
        handle.draft_goal(GoalSpec::new("new goal"), 3600),
        Err(RuntimeError::Busy { .. })
    ));
    // Inject a replacement to exercise stale callbacks even though the public
    // protocol refuses replacement until the previous run has exited.
    let new = handle
        .goal_ctx
        .goal_store
        .create(
            GoalSpec::new("new goal"),
            crate::runtime::goal::goal_environment(&cwd, &handle.goal_ctx.tool_surface),
        )
        .unwrap();
    sink.release.notify_one();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    let contaminated = outcome
        .usage_records
        .iter()
        .any(|r| r.billing_turn_id == old_bill);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    assert_eq!(outcome.id, new.id);
    assert!(!contaminated);
    assert_eq!(
        outcome.status,
        GoalStatus::Completed,
        "old sink error affected replacement: {:?}",
        outcome.reason
    );
}

#[tokio::test]
async fn audit4_empty_approval_is_rejected() {
    use crate::runtime::*;
    let (runtime, home, cwd, _) = goal_runtime("audit4-empty", false, None).await;
    let h = runtime.handle();
    let mut spec = GoalSpec::new("proof");
    spec.acceptance = GoalAcceptance::Human;
    h.confirmed_test_goal(spec).unwrap();
    h.start_goal_driver_and_run().unwrap();
    let g = h.wait_goal().await.unwrap();
    let result = h.approve_goal(&g.id, g.revision, vec![]);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    assert!(result.is_err(), "empty approval accepted");
}
#[tokio::test]
async fn audit4_verifier_attempt_exceeds_old_sixty_second_cap() {
    use crate::runtime::*;
    let verifier = Arc::new(HeldGoalVerifier {
        entered: Default::default(),
        release: Default::default(),
    });
    let (runtime, home, cwd, _) = goal_runtime("audit4-cap", false, Some(verifier.clone())).await;
    let h = runtime.handle();
    let mut spec = GoalSpec::new("proof");
    spec.acceptance = GoalAcceptance::Verifier {
        id: "held-check-v1".into(),
    };
    h.confirmed_test_goal(spec).unwrap();
    h.start_goal_driver_and_run().unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        verifier.entered.notified(),
    )
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(62)).await;
    let observed = h.goal_snapshot().unwrap().status;
    verifier.release.notify_one();
    h.wait_goal().await.unwrap();
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    assert_eq!(
        observed,
        GoalStatus::Active,
        "verification must not inherit the old sixty-second cap"
    );
}

/// A goal boundary must be typed, not something a host recognises by reading
/// the model-visible sentence.
#[tokio::test]
async fn goal_boundary_messages_are_typed_for_hosts() {
    use crate::runtime::*;
    let (runtime, home, cwd, _backend) = goal_runtime("goal-boundary", false, None).await;
    let handle = runtime.handle();
    let spec = GoalSpec::new("ship the parser");
    handle.confirmed_test_goal(spec).unwrap();
    handle.start_goal_driver_and_run().unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle.wait_goal()).await;
    // The session directory is under the home, not the working directory.
    let directory = handle
        .session_info()
        .conversation_path
        .parent()
        .unwrap()
        .to_path_buf();
    runtime.shutdown().await.unwrap();

    let reader = crate::runtime::session::SessionReader::new(directory);
    let rows = reader.conversation().unwrap();
    let mut kinds = Vec::new();
    for row in rows {
        let message = row.pointer("/_mink/goal_message");
        if let Some(message) = message {
            kinds.push((
                message["kind"].as_str().unwrap_or_default().to_string(),
                row.pointer("/_mink/source")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                message["objective"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                message["iteration"].as_u64().unwrap_or_default(),
            ));
        }
    }
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);

    assert!(
        !kinds.is_empty(),
        "goal rounds must publish a typed boundary"
    );
    assert_eq!(kinds[0].0, "start", "first boundary: {:?}", kinds[0]);
    // The source tag stays stable for every round so existing consumers keep
    // working; start versus continuation is carried by the typed `kind`.
    assert!(
        kinds.iter().all(|entry| entry.1 == "goal_continuation"),
        "{kinds:?}"
    );
    assert_eq!(
        kinds[0].2, "ship the parser",
        "objective travels structured"
    );
    assert_eq!(kinds[0].3, 1, "first boundary is round one");
    for entry in &kinds[1..] {
        assert_eq!(entry.0, "continuation", "later boundaries: {entry:?}");
    }
}

impl AgentRuntimeHandle {
    fn confirmed_test_goal(&self, mut spec: GoalSpec) -> RuntimeResult<GoalSnapshot> {
        spec.criteria = spec
            .acceptance_criteria
            .iter()
            .enumerate()
            .map(|(i, outcome)| GoalCriterion {
                id: format!("criterion-{}", i + 1),
                subgoal_id: "subgoal-1".into(),
                outcome: outcome.clone(),
                required_evidence: "Concrete proof".into(),
            })
            .collect();
        let duration = spec
            .limits
            .deadline_unix_secs
            .saturating_sub(crate::session::goal::goal_now())
            .max(1);
        let draft = self.draft_goal(spec, duration)?;
        self.confirm_goal(&draft.id, draft.revision)
    }
    fn confirmed_test_goal_with_lease(
        &self,
        mut spec: GoalSpec,
        lease: Box<dyn GoalRunLease>,
    ) -> RuntimeResult<GoalSnapshot> {
        spec.criteria = spec
            .acceptance_criteria
            .iter()
            .enumerate()
            .map(|(i, outcome)| GoalCriterion {
                id: format!("criterion-{}", i + 1),
                subgoal_id: "subgoal-1".into(),
                outcome: outcome.clone(),
                required_evidence: "Concrete proof".into(),
            })
            .collect();
        let duration = spec
            .limits
            .deadline_unix_secs
            .saturating_sub(crate::session::goal::goal_now())
            .max(1);
        let draft = self.draft_goal(spec, duration)?;
        self.confirm_and_start_goal(&draft.id, draft.revision, lease)
    }
}

fn natural_goal_end() -> Vec<anyhow::Result<LlmEvent>> {
    vec![
        Ok(LlmEvent::Text(LlmTextEvent {
            content: "execution round ended".into(),
        })),
        Ok(LlmEvent::Stop(LlmStopEvent {
            reason: "end_turn".into(),
        })),
    ]
}

fn mock_review_parts(request: &LlmRequest) -> Option<(GoalReviewCandidate, GoalSpec)> {
    if !request.tools.iter().any(|t| t["name"] == "GoalReview") {
        return None;
    }
    let text = request
        .messages
        .iter()
        .rev()
        .filter_map(|m| m["content"].as_str())
        .find(|s| !s.starts_with("<goal-conclusion-required>"))?;
    if !text.starts_with("<goal-review>") {
        return None;
    }
    let candidate =
        serde_json::from_str(text.lines().find_map(|l| l.strip_prefix("Candidate: "))?).unwrap();
    let contract =
        serde_json::from_str(text.lines().find_map(|l| l.strip_prefix("Contract: "))?).unwrap();
    Some((candidate, contract))
}

fn mock_goal_review(
    request: &LlmRequest,
    verdict: &str,
    dependency: &str,
) -> Vec<anyhow::Result<LlmEvent>> {
    let Some((candidate, spec)) = mock_review_parts(request) else {
        return natural_goal_end();
    };
    let has_result = request
        .messages
        .last()
        .and_then(|m| m["content"].as_array())
        .is_some_and(|blocks| {
            blocks.iter().any(|block| {
                block["type"] == "tool_result"
                    && block["_mink"]["tool_name"] == "GoalReview"
                    && block["content"].as_str() == Some("Host goal conclusion recorded.")
            })
        });
    if has_result {
        return natural_goal_end();
    }
    let pass = verdict == "satisfied";
    let review = serde_json::json!({
        "candidate":candidate,"verdict":verdict,"original_request":{"status":if pass {"pass"} else {"unknown"},"reason":"checked the whole original request","evidence":if pass {vec!["proof.txt"]} else {vec![]} },"summary":"independent criterion review","next_step":if pass { "" } else { "continue with the unmet criterion" },
        "criteria":spec.criteria.iter().map(|c| serde_json::json!({"criterion_id":c.id,"status":if pass {"pass"} else {"unknown"},"reason":"checked available evidence","evidence":if pass {vec!["proof.txt"]} else {vec![]} })).collect::<Vec<_>>(),
        "constraints":spec.constraints.iter().map(|c| serde_json::json!({"constraint":c,"status":if pass {"pass"} else {"unknown"},"reason":"checked constraints","evidence":if pass {vec!["proof.txt"]} else {vec![]} })).collect::<Vec<_>>(),
        "blocker":if verdict == "blocked" {serde_json::json!({"category":"external-dependency","criterion_ids":spec.criteria.iter().map(|c|c.id.clone()).collect::<Vec<_>>(),"dependency":dependency,"evidence":["proof.txt"]})} else {serde_json::Value::Null}
    });
    let mut events = Vec::new();
    if pass || verdict == "blocked" {
        events.push(Ok(LlmEvent::ToolCall(crate::protocol::ToolCallEvent {
            id: "review-read".into(),
            name: "Read".into(),
            input_json: serde_json::json!({"path":"proof.txt"}),
            fields: Default::default(),
            parse_error: None,
            raw_arguments_digest: None,
        })));
    }
    events.push(Ok(LlmEvent::ToolCall(crate::protocol::ToolCallEvent {
        id: "review-submit".into(),
        name: "GoalReview".into(),
        input_json: review,
        fields: Default::default(),
        parse_error: None,
        raw_arguments_digest: None,
    })));
    events.push(Ok(LlmEvent::Stop(LlmStopEvent {
        reason: "tool_use".into(),
    })));
    events
}

#[tokio::test]
async fn goal_human_draft_requires_explicit_selection_with_multiple_checkers() {
    struct NamedChecker(&'static str);
    #[async_trait::async_trait]
    impl crate::runtime::GoalVerifier for NamedChecker {
        fn id(&self) -> &str {
            self.0
        }
        async fn verify(
            &self,
            _: crate::runtime::GoalVerificationContext,
        ) -> anyhow::Result<crate::runtime::GoalVerification> {
            anyhow::bail!("not executed in a draft")
        }
    }
    let home = unique_temp_dir("goal-multiple-checkers-home");
    let cwd = unique_temp_dir("goal-multiple-checkers-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let runtime = AgentRuntime::start(
        AgentOptions::new(&home, &cwd)
            .with_llm_backend(Arc::new(GoalReportBackend {
                calls: Default::default(),
                continue_first: false,
            }))
            .with_goal_verifier(Arc::new(NamedChecker("first")))
            .with_goal_verifier(Arc::new(NamedChecker("second"))),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let mut spec = GoalSpec::new("ship");
    spec.acceptance = crate::runtime::GoalAcceptance::Human;
    assert!(handle.draft_goal(spec.clone(), 3600).is_err());
    assert!(handle.goal_snapshot().is_none());
    spec.checker_id = Some("first".into());
    assert_eq!(
        handle
            .draft_goal(spec, 3600)
            .unwrap()
            .spec
            .checker_id
            .as_deref(),
        Some("first")
    );
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_draft_discussion_is_internal_and_does_not_register_a_human_request() {
    let (runtime, home, cwd, backend) = goal_runtime("goal-draft-discussion", false, None).await;
    let handle = runtime.handle();
    let draft = handle
        .request_goal_draft(GoalSpec::new("ship the parser"), 3600)
        .unwrap();
    assert!(!draft.draft_ready);
    assert!(handle.confirm_goal(&draft.id, draft.revision).is_err());
    assert!(
        handle
            .generate_goal_draft_at(&draft.id, draft.revision + 1, "stale feedback")
            .await
            .is_err()
    );
    assert_eq!(backend.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(
        handle.goal_draft_snapshot().unwrap().revision,
        draft.revision
    );
    let discussed = handle
        .generate_goal_draft("also cover invalid input")
        .await
        .unwrap();
    assert_eq!(discussed.status, crate::runtime::GoalStatus::Draft);
    assert!(discussed.iterations.is_empty());
    assert!(handle.input_inbox.entries().is_empty());
    assert!(handle.input_inbox.active_turn().is_none());
    let reader = crate::runtime::session::SessionReader::new(
        handle.session_info().conversation_path.parent().unwrap(),
    );
    let rows = reader.conversation().unwrap();
    let row = rows
        .iter()
        .find(|row| row.pointer("/_mink/source").and_then(|v| v.as_str()) == Some("goal_draft"))
        .unwrap();
    assert_eq!(
        row.pointer("/_mink/internal"),
        Some(&serde_json::json!(true))
    );
    assert_eq!(
        row.pointer("/_mink/goal_message/kind")
            .and_then(|v| v.as_str()),
        Some("draft")
    );
    assert_eq!(
        row.pointer("/_mink/goal_message/feedback")
            .and_then(|v| v.as_str()),
        Some("also cover invalid input")
    );
    assert_eq!(rows.iter().filter(|row| row["role"] == "user").count(), 1);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_draft_rejects_mutation_and_requires_host_confirmation() {
    let (runtime, home, cwd, backend) = goal_runtime("goal-draft-authority", false, None).await;
    let handle = runtime.handle();
    assert!(handle.create_goal(GoalSpec::new("ship")).is_err());
    let draft = handle.draft_goal(GoalSpec::new("ship"), 3600).unwrap();
    handle.start_goal_driver_and_run().unwrap();
    tokio::task::yield_now().await;
    assert_eq!(backend.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    let tools = crate::tools::runner::ToolRunner::new(Arc::new(crate::context::ToolContext::from(
        handle.goal_ctx.as_ref(),
    )));
    let call = crate::protocol::ToolCallEvent {
        id: "forbidden-write".into(),
        name: "Write".into(),
        input_json: serde_json::json!({"path":"forbidden.txt","content":"bad"}),
        fields: Default::default(),
        parse_error: None,
        raw_arguments_digest: None,
    };
    let result = tools.execute_all(vec![call]).await.unwrap();
    assert!(!result[0].succeeded());
    assert!(!cwd.join("forbidden.txt").exists());
    assert!(handle.confirm_goal(&draft.id, draft.revision + 1).is_err());
    handle.confirm_goal(&draft.id, draft.revision).unwrap();
    let completed = handle.wait_goal().await.unwrap();
    assert_eq!(
        completed.completion_basis,
        Some(crate::runtime::GoalCompletionBasis::AgentReviewed)
    );
    assert!(completed.report.is_none());
    assert_eq!(completed.iterations.len(), 1);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_review_rejects_unread_forged_or_changed_evidence() {
    use crate::session::goal::*;
    let (runtime, home, cwd, _) = goal_runtime("goal-review-evidence", false, None).await;
    let handle = runtime.handle();
    let goal = handle
        .goal_ctx
        .goal_store
        .create(
            GoalSpec::new("ship"),
            crate::runtime::goal::goal_environment(&cwd, &handle.goal_ctx.tool_surface),
        )
        .unwrap();
    let candidate = GoalReviewCandidate {
        generation: 0,
        goal_id: goal.id.clone(),
        contract_revision: goal.contract_revision,
        iteration: 1,
        conversation_seq: 0,
        reviewer_id: "test-reviewer".into(),
    };
    let goal = handle
        .goal_ctx
        .goal_store
        .change(&goal.id, goal.revision, |g| {
            g.review_candidate = Some(candidate.clone());
            g.spec.constraints = vec!["only the requested output".into()];
            Ok(())
        })
        .unwrap();
    let session = Arc::new(crate::runtime::goal::review::GoalReviewSession::new(
        goal.clone(),
        handle.goal_ctx.goal_store.clone(),
        handle.goal_ctx.usage.clone(),
    ));
    let mut ctx = crate::context::ToolContext::from(handle.goal_ctx.as_ref());
    ctx.goal_adjudication = Some(session.clone());
    let review = GoalReview {
        candidate,
        verdict: GoalReviewVerdict::Satisfied,
        report: None,
        original_request: Some(GoalRequestReview {
            status: GoalCriterionStatus::Pass,
            reason: "checked original user request".into(),
            evidence: vec!["proof.txt".into()],
        }),
        criteria: vec![GoalCriterionReview {
            criterion_id: "criterion-1".into(),
            status: GoalCriterionStatus::Pass,
            reason: "verified".into(),
            evidence: vec!["proof.txt".into()],
        }],
        constraints: vec![GoalConstraintReview {
            constraint: "only the requested output".into(),
            status: GoalCriterionStatus::Pass,
            reason: "inspected output".into(),
            evidence: vec!["proof.txt".into()],
        }],
        summary: "complete".into(),
        next_step: String::new(),
        blocker: None,
    };
    assert!(
        session.submit(review.clone(), &ctx).is_err(),
        "file existence is not inspection"
    );
    let call = crate::protocol::ToolCallEvent {
        id: "read".into(),
        name: "Read".into(),
        input_json: serde_json::json!({"path":"proof.txt"}),
        fields: Default::default(),
        parse_error: None,
        raw_arguments_digest: None,
    };
    session
        .finish_read(session.capture_read(&call, &ctx).unwrap().unwrap())
        .unwrap();
    let host_goal_before_submission = handle.goal_snapshot().unwrap();
    for bad in [
        {
            let mut r = review.clone();
            r.constraints[0].evidence.clear();
            r
        },
        {
            let mut r = review.clone();
            r.original_request.as_mut().unwrap().reason.clear();
            r
        },
        {
            let mut r = review.clone();
            r.criteria.clear();
            r
        },
        {
            let mut r = review.clone();
            r.criteria.push(r.criteria[0].clone());
            r
        },
        {
            let mut r = review.clone();
            r.criteria[0].criterion_id = "invented".into();
            r
        },
        {
            let mut r = review.clone();
            r.criteria[0].evidence.clear();
            r
        },
        {
            let mut r = review.clone();
            r.candidate.contract_revision += 1;
            r
        },
    ] {
        assert!(session.submit(bad, &ctx).is_err());
    }
    assert!(
        serde_json::from_value::<GoalReview>(serde_json::to_value(&review).unwrap()).is_ok(),
        "the host tool protocol accepts its explicit verdict"
    );
    session.submit(review.clone(), &ctx).unwrap();
    assert_eq!(
        handle.goal_snapshot().unwrap(),
        host_goal_before_submission,
        "recording the host conclusion must not bypass the runtime commit"
    );
    assert!(session.submission.lock().unwrap().is_some());
    std::fs::write(cwd.join("proof.txt"), "changed after inspection").unwrap();
    assert!(session.submit(review, &ctx).is_err());
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}
