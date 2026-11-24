use super::*;

use axum::body::Body;

use axum::http::{Method, Request, StatusCode};

use http_body_util::BodyExt;

use std::path::PathBuf;

use tower::ServiceExt;

pub(super) struct MockRuntimeBackend;

#[async_trait::async_trait]
impl mink::runtime::LlmBackend for MockRuntimeBackend {
    fn name(&self) -> &str {
        "server-api-mock"
    }

    async fn stream(
        &self,
        _request: mink::runtime::LlmRequest,
    ) -> anyhow::Result<mink::runtime::LlmResponseStream> {
        Ok(mink::runtime::LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![
                Ok(mink::runtime::LlmEvent::Text(mink::runtime::LlmTextEvent {
                    content: "mock api answer".into(),
                })),
                Ok(mink::runtime::LlmEvent::Stop(mink::runtime::LlmStopEvent {
                    reason: "end_turn".into(),
                })),
            ])),
            attempt_count: 1,
        })
    }
}

/// 临时 home + 一个含 3 行 conversation 的会话 + 1 个 artifact。
/// 每个调用使用唯一 home（进程内并行测试共享同一 pid 的固定目录会
/// 互相覆盖 artifact/会话文件，造成偶发竞态失败）。
pub(super) fn test_router() -> Router {
    test_router_with_session_dir().0
}

/// Same fixture, but also hands back the session directory so a test can
/// seed authoritative state files before requesting them.
pub(super) fn test_router_with_session_dir() -> (Router, PathBuf) {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let home = std::env::temp_dir().join(format!(
        "mink-server-itest-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    let sess = home
        .join(".mink")
        .join("projects")
        .join("proj")
        .join("test-session");
    std::fs::create_dir_all(&sess).unwrap();
    std::fs::write(
        sess.join("session.json"),
        serde_json::json!({
            "id": "test-session", "alias": null, "title": "t", "cwd": sess.display().to_string(),
            "created_at": "", "updated_at": "", "parent": null, "first_prompt": null, "summary": null
        }).to_string(),
    ).unwrap();
    std::fs::write(
        sess.join("conversation.jsonl"),
        "{\"role\":\"user\",\"content\":\"one\"}\n{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"two\"}]}\n{\"role\":\"user\",\"content\":\"three\"}\n",
    ).unwrap();
    std::fs::create_dir_all(sess.join("artifacts")).unwrap();
    std::fs::write(sess.join("artifacts/abc.txt"), "artifact body").unwrap();

    let registry = Arc::new(Registry::new(home, "flash".to_string(), 4));
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let state = Arc::new(ApiState {
        registry,
        cwd: std::env::temp_dir(),
        shutdown,
    });
    (router(state), sess)
}

/// A router holding one session that was created and opened through the
/// real API against a mock backend, so goal control has a live runtime to
/// drive. Returns the router and the fixture home for cleanup.
///
/// The session is created through the API rather than hand-placed on disk:
/// the runtime derives the project key by hashing the working directory, so
/// a hand-written `projects/<name>` layout would never resolve.
pub(super) async fn open_goal_router() -> (Router, PathBuf, String) {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let home = std::env::temp_dir().join(format!(
        "mink-server-goaltest-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    let cwd = home.join("workspace");
    std::fs::create_dir_all(&cwd).unwrap();
    let registry = Arc::new(Registry::with_llm_backend(
        home.clone(),
        "flash".to_string(),
        4,
        Arc::new(MockRuntimeBackend),
    ));
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let app = router(Arc::new(ApiState {
        registry,
        cwd: cwd.clone(),
        shutdown,
    }));

    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "name": "goaltest", "cwd": cwd.display().to_string() })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .expect("create request must be served");
    let status = created.status();
    let bytes = created.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(status, StatusCode::OK, "create: {body}");
    let session_id = body["data"]["id"].as_str().expect("session id").to_string();

    let opened = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/sessions/{session_id}/open"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("open request must be served");
    let status = opened.status();
    let bytes = opened.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        status,
        StatusCode::OK,
        "open: {}",
        String::from_utf8_lossy(&bytes)
    );
    (app, home, session_id)
}

pub(super) async fn post_json(
    app: &Router,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// Like [`post_json`], but the body is sent verbatim so a test can exercise
/// values the JSON value type cannot express (overflow) and assert a body
/// that is not an API envelope (decoder rejection).
pub(super) async fn post_raw(app: &Router, uri: &str, body: &str) -> (StatusCode, String) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// One router over a fresh home, for tests that create real sessions.
pub(super) fn creation_router() -> (Router, PathBuf, PathBuf) {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let home = std::env::temp_dir().join(format!(
        "mink-server-create-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    let cwd = home.join("workspace");
    std::fs::create_dir_all(&cwd).unwrap();
    let registry = Arc::new(Registry::new(home.clone(), "flash".to_string(), 4));
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let app = router(Arc::new(ApiState {
        registry,
        cwd: cwd.clone(),
        shutdown,
    }));
    (app, home, cwd)
}

/// Minimal query escaping for a temp path: enough for spaces and separators.
pub(super) fn urlencoding_simple(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                vec![byte as char]
            }
            other => format!("%{other:02X}").chars().collect(),
        })
        .collect()
}

/// Observable form of the `max_running` contract: how many provider
/// streams are in flight right now, and the high-water mark seen. A
/// sequence of host commands may never push the high-water mark above the
/// configured limit.
#[derive(Default)]
pub(super) struct CapacityGauge {
    pub(super) active: std::sync::atomic::AtomicUsize,
    pub(super) max_active: std::sync::atomic::AtomicUsize,
    pub(super) calls: std::sync::atomic::AtomicUsize,
}

impl CapacityGauge {
    fn active(&self) -> usize {
        self.active.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn max_active(&self) -> usize {
        self.max_active.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Backend whose stream waits for a signal, so an execution can hold its
/// capacity slot while the host makes competing decisions.
pub(super) struct GatedBackend {
    pub(super) release: Arc<tokio::sync::watch::Sender<bool>>,
    pub(super) gauge: Arc<CapacityGauge>,
}

impl GatedBackend {
    fn new(release: &Arc<tokio::sync::watch::Sender<bool>>) -> Self {
        Self {
            release: release.clone(),
            gauge: Arc::new(CapacityGauge::default()),
        }
    }

    fn with_gauge(
        release: &Arc<tokio::sync::watch::Sender<bool>>,
        gauge: Arc<CapacityGauge>,
    ) -> Self {
        Self {
            release: release.clone(),
            gauge,
        }
    }
}

#[async_trait::async_trait]
impl mink::runtime::LlmBackend for GatedBackend {
    fn name(&self) -> &str {
        "server-api-gated"
    }

    async fn stream(
        &self,
        _request: mink::runtime::LlmRequest,
    ) -> anyhow::Result<mink::runtime::LlmResponseStream> {
        use std::sync::atomic::Ordering::SeqCst;
        let in_flight = self.gauge.active.fetch_add(1, SeqCst) + 1;
        self.gauge.max_active.fetch_max(in_flight, SeqCst);
        self.gauge.calls.fetch_add(1, SeqCst);
        let result: anyhow::Result<()> = async {
            let mut released = self.release.subscribe();
            while !*released.borrow() {
                released
                    .changed()
                    .await
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
            }
            Ok(())
        }
        .await;
        self.gauge.active.fetch_sub(1, SeqCst);
        result?;
        Ok(mink::runtime::LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![
                Ok(mink::runtime::LlmEvent::Text(mink::runtime::LlmTextEvent {
                    content: "gated answer".into(),
                })),
                Ok(mink::runtime::LlmEvent::Stop(mink::runtime::LlmStopEvent {
                    reason: "end_turn".into(),
                })),
            ])),
            attempt_count: 1,
        })
    }
}

pub(super) async fn gated_router(
    max_running: usize,
    release: Arc<tokio::sync::watch::Sender<bool>>,
) -> (Router, PathBuf, String, String) {
    let home = std::env::temp_dir().join(format!(
        "mink-server-gated-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let cwd = home.join("workspace");
    std::fs::create_dir_all(&cwd).unwrap();
    let registry = Arc::new(Registry::with_llm_backend(
        home.clone(),
        "flash".to_string(),
        max_running,
        Arc::new(GatedBackend::new(&release)),
    ));
    let first = registry.create("first", &cwd).await.unwrap();
    let second = registry.create("second", &cwd).await.unwrap();
    registry
        .open(&first.id, Some(&first.project_key))
        .await
        .unwrap();
    registry
        .open(&second.id, Some(&second.project_key))
        .await
        .unwrap();
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let app = router(Arc::new(ApiState {
        registry,
        cwd,
        shutdown,
    }));
    (app, home, first.id, second.id)
}

pub(super) async fn wait_for_running(app: &Router, id: &str, expected: bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let (_, body) = req(app, Method::GET, &format!("/api/sessions/{id}")).await;
        if body["data"]["running"].as_bool() == Some(expected) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "session {id} never reached running={expected}: {body}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

pub(super) async fn req(
    app: &Router,
    method: Method,
    uri: &str,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    (status, json)
}

mod goals;
mod inputs;
mod resources;
mod stream;
