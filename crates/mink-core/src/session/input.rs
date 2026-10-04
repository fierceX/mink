//! Durable human input admission and safe-boundary delivery.
use crate::session::persistence::{PersistenceFault, publish_state};
use crate::session::store::ConversationStore;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HumanInput {
    pub request_id: String,
    pub text: String,
    #[serde(default)]
    pub target_turn_id: Option<String>,
    #[serde(default)]
    pub attachment_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InputStatus {
    Pending,
    Applying,
    Applied,
    Unapplied,
    Withdrawn,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputReceipt {
    pub input_id: String,
    pub revision: u64,
    pub turn_id: String,
    pub status: InputStatus,
    pub guidance: bool,
    pub input: HumanInput,
    // Immutable request payload for idempotency even after an edit.
    original: HumanInput,
}

impl InputReceipt {
    pub(crate) fn transient(input: HumanInput, turn_id: &str) -> Self {
        Self {
            input_id: input.request_id.clone(),
            revision: 0,
            turn_id: turn_id.into(),
            status: InputStatus::Pending,
            guidance: false,
            original: input.clone(),
            input,
        }
    }
}

#[derive(Default, Clone, Serialize, Deserialize)]
struct Snapshot {
    entries: Vec<InputReceipt>,
}
#[derive(Default)]
struct State {
    snapshot: Snapshot,
    active: Option<String>,
}

pub struct InputInbox {
    path: PathBuf,
    state: Mutex<State>,
    fault: PersistenceFault,
    changes: tokio::sync::watch::Sender<Arc<Vec<InputReceipt>>>,
}
impl InputInbox {
    pub(crate) fn load(path: PathBuf, fault: PersistenceFault) -> Result<Self> {
        let snapshot = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Snapshot::default(),
            Err(e) => return Err(e.into()),
        };
        let (changes, _) = tokio::sync::watch::channel(Arc::new(outstanding_entries(&snapshot)));
        Ok(Self {
            changes,
            path,
            state: Mutex::new(State {
                snapshot,
                active: None,
            }),
            fault,
        })
    }
    fn publish(&self, state: &mut State, snapshot: Snapshot) -> Result<()> {
        publish_state(&self.path, &serde_json::to_vec(&snapshot)?, &self.fault)?;
        state.snapshot = snapshot;
        self.changes
            .send_replace(Arc::new(outstanding_entries(&state.snapshot)));
        Ok(())
    }
    pub fn entries(&self) -> Vec<InputReceipt> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot
            .entries
            .clone()
    }
    /// Current outstanding inputs and subsequent durable publications. Subscription
    /// and initial snapshot share the admission lock; failed writes never notify.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<Arc<Vec<InputReceipt>>> {
        let _state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.changes.subscribe()
    }
    pub fn outstanding(&self) -> Vec<InputReceipt> {
        self.changes.borrow().as_ref().clone()
    }
    pub fn active_turn(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active
            .clone()
    }
    pub fn existing(&self, input: &HumanInput) -> Result<Option<InputReceipt>> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = state
            .snapshot
            .entries
            .iter()
            .find(|e| e.input_id == input.request_id)
        {
            if &entry.original != input {
                bail!("request ID conflicts with an existing input");
            }
            return Ok(Some(entry.clone()));
        }
        Ok(None)
    }
    pub(crate) fn accept(&self, input: HumanInput, new_turn: Option<&str>) -> Result<InputReceipt> {
        self.fault.check()?;
        validate(&input)?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = state
            .snapshot
            .entries
            .iter()
            .find(|e| e.input_id == input.request_id)
        {
            if entry.original != input {
                bail!("request ID conflicts with an existing input");
            }
            return Ok(entry.clone());
        }
        if state
            .snapshot
            .entries
            .iter()
            .filter(|e| {
                matches!(
                    e.status,
                    InputStatus::Pending | InputStatus::Applying | InputStatus::Unapplied
                )
            })
            .count()
            >= 32
        {
            bail!("input inbox is full (max 32 outstanding inputs)");
        }
        let guidance = input.target_turn_id.is_some();
        let turn_id = if let Some(target) = &input.target_turn_id {
            if state.active.as_ref() != Some(target) {
                bail!("target turn no longer accepts guidance");
            }
            target.clone()
        } else {
            if state.active.is_some() {
                bail!("a new turn cannot start while another turn is active");
            }
            new_turn
                .ok_or_else(|| anyhow::anyhow!("new turn ID required"))?
                .to_string()
        };
        let receipt = InputReceipt {
            input_id: input.request_id.clone(),
            revision: 1,
            turn_id: turn_id.clone(),
            status: InputStatus::Pending,
            guidance,
            original: input.clone(),
            input,
        };
        let mut snapshot = state.snapshot.clone();
        snapshot.entries.push(receipt.clone());
        self.publish(&mut state, snapshot)?;
        state.active = Some(turn_id);
        Ok(receipt)
    }
    pub fn update(&self, id: &str, revision: u64, text: Option<String>) -> Result<InputReceipt> {
        self.fault.check()?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut snapshot = state.snapshot.clone();
        let entry = snapshot
            .entries
            .iter_mut()
            .find(|e| e.input_id == id)
            .ok_or_else(|| anyhow::anyhow!("input not found"))?;
        if entry.revision != revision {
            bail!("stale input revision");
        }
        if !matches!(entry.status, InputStatus::Pending | InputStatus::Unapplied) {
            bail!("input has already been consumed or withdrawn");
        }
        if let Some(text) = text {
            entry.input.text = text;
            validate(&entry.input)?;
        } else {
            entry.status = InputStatus::Withdrawn;
        }
        entry.revision += 1;
        let receipt = entry.clone();
        self.publish(&mut state, snapshot)?;
        Ok(receipt)
    }
    pub(crate) fn resume(&self, id: &str, revision: u64, turn_id: &str) -> Result<InputReceipt> {
        self.fault.check()?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active.is_some() {
            bail!("session is running");
        }
        let mut snapshot = state.snapshot.clone();
        let entry = snapshot
            .entries
            .iter_mut()
            .find(|e| e.input_id == id)
            .ok_or_else(|| anyhow::anyhow!("input not found"))?;
        if entry.revision != revision || entry.status != InputStatus::Unapplied {
            bail!("input cannot be resumed at this revision");
        }
        entry.status = InputStatus::Pending;
        entry.turn_id = turn_id.to_string();
        entry.guidance = false;
        entry.revision += 1;
        let receipt = entry.clone();
        self.publish(&mut state, snapshot)?;
        state.active = Some(turn_id.to_string());
        Ok(receipt)
    }
    /// Applying is published before append. A failed/uncertain append latches
    /// the session; restart checks the formal history before retrying.
    pub(crate) async fn apply_pending(
        &self,
        store: &ConversationStore,
        interrupted: impl Fn() -> bool,
    ) -> Result<usize> {
        let mut applied = 0;
        loop {
            self.fault.check()?;
            if interrupted() {
                return Ok(applied);
            }
            let receipt = {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if interrupted() {
                    return Ok(applied);
                }
                let mut snapshot = state.snapshot.clone();
                let Some(entry) = snapshot.entries.iter_mut().find(|e| {
                    e.status == InputStatus::Pending && Some(&e.turn_id) == state.active.as_ref()
                }) else {
                    return Ok(applied);
                };
                entry.status = InputStatus::Applying;
                entry.revision += 1;
                let receipt = entry.clone();
                self.publish(&mut state, snapshot)?;
                receipt
            };
            let message = serde_json::json!({"role":"user", "content":receipt.input.text, "_mink": {
                "input_id":receipt.input_id, "turn_id":receipt.turn_id, "guidance":receipt.guidance, "attachment_ids":receipt.input.attachment_ids
            }});
            if let Err(error) = store.append_runtime_message_durable(message).await {
                return Err(self.fault.raise(
                    &self.path,
                    format!("input history append uncertain: {error:#}"),
                ));
            }
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let mut snapshot = state.snapshot.clone();
            let entry = snapshot
                .entries
                .iter_mut()
                .find(|e| e.input_id == receipt.input_id)
                .expect("applying input remains present");
            entry.status = InputStatus::Applied;
            entry.revision += 1;
            if let Err(error) = self.publish(&mut state, snapshot) {
                return Err(self.fault.raise(
                    &self.path,
                    format!("input applied but receipt publish failed: {error:#}"),
                ));
            }
            applied += 1;
        }
    }
    /// Same mutex as admission: either pending work wins or turn completion
    /// closes admission. A late request is never implicitly a new turn.
    pub(crate) fn close_if_empty(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .snapshot
            .entries
            .iter()
            .any(|e| e.status == InputStatus::Pending && Some(&e.turn_id) == state.active.as_ref())
        {
            return false;
        }
        state.active = None;
        true
    }
    pub(crate) fn finish(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active = None;
        let mut snapshot = state.snapshot.clone();
        let mut changed = false;
        for entry in &mut snapshot.entries {
            if entry.status == InputStatus::Pending {
                entry.status = InputStatus::Unapplied;
                entry.revision += 1;
                changed = true;
            }
        }
        if changed {
            self.publish(&mut state, snapshot)?;
        }
        Ok(())
    }
    pub(crate) async fn recover(&self, store: &ConversationStore) -> Result<()> {
        let history = ConversationStore::new(store.path().clone()).lines().await?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut snapshot = state.snapshot.clone();
        let mut changed = false;
        for entry in &mut snapshot.entries {
            if matches!(entry.status, InputStatus::Pending | InputStatus::Applying) {
                let matches: Vec<_> = history
                    .iter()
                    .filter(|m| {
                        m.pointer("/_mink/input_id")
                            .and_then(serde_json::Value::as_str)
                            == Some(&entry.input_id)
                    })
                    .collect();
                if matches.len() > 1 {
                    bail!("duplicate human input in formal history");
                }
                if let Some(message) = matches.first() {
                    let expected = serde_json::json!({"role":"user","content":entry.input.text,"_mink":{
                        "input_id":entry.input_id,"turn_id":entry.turn_id,"guidance":entry.guidance,"attachment_ids":entry.input.attachment_ids
                    }});
                    if **message != expected {
                        bail!("human input history does not match applying receipt");
                    }
                }
                entry.status = if matches.is_empty() {
                    InputStatus::Unapplied
                } else {
                    InputStatus::Applied
                };
                entry.revision += 1;
                changed = true;
            }
        }
        if changed {
            self.publish(&mut state, snapshot)?;
        }
        Ok(())
    }
}
fn outstanding_entries(snapshot: &Snapshot) -> Vec<InputReceipt> {
    snapshot
        .entries
        .iter()
        .filter(|entry| {
            matches!(
                entry.status,
                InputStatus::Pending | InputStatus::Applying | InputStatus::Unapplied
            )
        })
        .cloned()
        .collect()
}

fn validate(input: &HumanInput) -> Result<()> {
    if input.request_id.is_empty()
        || input.request_id.len() > 128
        || !input
            .request_id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.:".contains(&c))
    {
        bail!("invalid request ID");
    }
    if input.text.trim().is_empty() || input.text.len() > 128 * 1024 {
        bail!("input must contain text and be at most 128 KiB");
    }
    if input.attachment_ids.len() > 8 {
        bail!("max 8 attachments");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "mink-inbox-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn input(id: &str, turn: Option<&str>) -> HumanInput {
        HumanInput {
            request_id: id.into(),
            text: id.into(),
            target_turn_id: turn.map(str::to_string),
            attachment_ids: vec![],
        }
    }
    #[tokio::test]
    async fn subscription_tracks_only_durable_publications() {
        let dir = TestDir::new();
        let path = dir.path().join("inputs.json");
        let inbox = InputInbox::load(path.clone(), PersistenceFault::default()).unwrap();
        let mut changes = inbox.subscribe();
        assert!(changes.borrow_and_update().is_empty());
        let receipt = inbox.accept(input("first", None), Some("turn")).unwrap();
        assert!(changes.has_changed().unwrap());
        assert_eq!(changes.borrow_and_update()[0].revision, receipt.revision);
        let late = inbox.subscribe();
        assert_eq!(late.borrow()[0].input_id, "first");
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(inbox.update("first", receipt.revision, None).is_err());
        assert!(!changes.has_changed().unwrap());
        assert_eq!(changes.borrow()[0].status, InputStatus::Pending);
    }

    #[tokio::test]
    async fn delivery_is_ordered_idempotent_and_terminal_admission_is_closed() {
        let dir = TestDir::new();
        let store = ConversationStore::new(dir.path().join("conversation.jsonl"));
        store.ensure().await.unwrap();
        let inbox =
            InputInbox::load(dir.path().join("inputs.json"), PersistenceFault::default()).unwrap();
        inbox.accept(input("first", None), Some("turn")).unwrap();
        let receipt = inbox.accept(input("guide", Some("turn")), None).unwrap();
        assert_eq!(
            inbox
                .accept(input("guide", Some("turn")), None)
                .unwrap()
                .revision,
            receipt.revision
        );
        let mut conflict = input("guide", Some("turn"));
        conflict.text = "different".into();
        assert!(inbox.existing(&conflict).is_err());
        assert!(!inbox.close_if_empty());
        assert_eq!(inbox.apply_pending(&store, || false).await.unwrap(), 2);
        assert!(
            inbox
                .update("guide", receipt.revision, Some("late".into()))
                .is_err()
        );
        assert_eq!(store.lines().await.unwrap().len(), 2);
        assert!(inbox.close_if_empty());
        assert!(inbox.accept(input("late", Some("turn")), None).is_err());
    }
    #[tokio::test]
    async fn restart_reconciles_applying_without_duplicate_history() {
        let dir = TestDir::new();
        let path = dir.path().join("inputs.json");
        let store = ConversationStore::new(dir.path().join("conversation.jsonl"));
        store.ensure().await.unwrap();
        let inbox = InputInbox::load(path.clone(), PersistenceFault::default()).unwrap();
        inbox.accept(input("first", None), Some("turn")).unwrap();
        inbox.apply_pending(&store, || false).await.unwrap();
        inbox.accept(input("unused", Some("turn")), None).unwrap();
        // Simulate crash after durable append, before applied publication.
        {
            let mut state = inbox.state.lock().unwrap();
            let mut snapshot = state.snapshot.clone();
            snapshot.entries[0].status = InputStatus::Applying;
            inbox.publish(&mut state, snapshot).unwrap();
        }
        let restored = InputInbox::load(path, PersistenceFault::default()).unwrap();
        restored.recover(&store).await.unwrap();
        assert_eq!(restored.entries()[0].status, InputStatus::Applied);
        assert_eq!(restored.entries()[1].status, InputStatus::Unapplied);
        let revision = restored.entries()[1].revision;
        restored.resume("unused", revision, "new").unwrap();
        restored.apply_pending(&store, || false).await.unwrap();
        assert_eq!(store.lines().await.unwrap().len(), 2);
    }
    #[tokio::test]
    async fn revisions_limits_cancellation_and_edit_withdraw_race_are_fail_closed() {
        let dir = TestDir::new();
        let store = ConversationStore::new(dir.path().join("conversation.jsonl"));
        store.ensure().await.unwrap();
        let inbox = std::sync::Arc::new(
            InputInbox::load(dir.path().join("inputs.json"), PersistenceFault::default()).unwrap(),
        );
        let receipt = inbox.accept(input("first", None), Some("turn")).unwrap();
        let edited = inbox
            .update("first", receipt.revision, Some("edited".into()))
            .unwrap();
        assert!(inbox.update("first", receipt.revision, None).is_err());
        assert_eq!(inbox.apply_pending(&store, || true).await.unwrap(), 0);
        assert!(store.lines().await.unwrap().is_empty());
        assert_eq!(
            inbox
                .existing(&input("first", None))
                .unwrap()
                .unwrap()
                .input
                .text,
            "edited"
        );
        let race = inbox.clone();
        let handle = std::thread::spawn(move || race.update("first", edited.revision, None));
        let competing = inbox.update("first", edited.revision, Some("race edit".into()));
        assert_ne!(handle.join().unwrap().is_ok(), competing.is_ok());
        for n in 0..31 {
            inbox
                .accept(input(&format!("guide-{n}"), Some("turn")), None)
                .unwrap();
        }
        // first may have been withdrawn, so fill its slot when necessary.
        if inbox.entries()[0].status == InputStatus::Withdrawn {
            inbox
                .accept(input("replacement", Some("turn")), None)
                .unwrap();
        }
        assert!(inbox.accept(input("overflow", Some("turn")), None).is_err());
        inbox.finish().unwrap();
        assert!(inbox.entries().iter().all(|entry| matches!(
            entry.status,
            InputStatus::Unapplied | InputStatus::Withdrawn
        )));
    }
    #[tokio::test]
    async fn receipt_publish_after_history_failure_latches_and_restart_reconciles() {
        let dir = TestDir::new();
        let path = dir.path().join("inputs.json");
        let fault = PersistenceFault::default();
        let inbox = InputInbox::load(path.clone(), fault.clone()).unwrap();
        let store = ConversationStore::new(dir.path().join("conversation.jsonl"));
        store.ensure().await.unwrap();
        inbox.accept(input("first", None), Some("turn")).unwrap();
        let saved = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let saved_copy = saved.clone();
        let path_copy = path.clone();
        store
            .observe_commits(std::sync::Arc::new(move |_, _| {
                *saved_copy.lock().unwrap() = std::fs::read(&path_copy).unwrap();
                std::fs::remove_file(&path_copy).unwrap();
                std::fs::create_dir(&path_copy).unwrap();
            }))
            .await
            .unwrap();
        assert!(inbox.apply_pending(&store, || false).await.is_err());
        assert!(fault.check().is_err());
        assert!(inbox.accept(input("late", Some("turn")), None).is_err());
        assert_eq!(store.lines().await.unwrap().len(), 1);
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, &*saved.lock().unwrap()).unwrap();
        let restored = InputInbox::load(path, PersistenceFault::default()).unwrap();
        restored.recover(&store).await.unwrap();
        assert_eq!(restored.entries()[0].status, InputStatus::Applied);
        assert_eq!(restored.apply_pending(&store, || false).await.unwrap(), 0);
    }
    #[tokio::test]
    async fn guidance_admission_and_success_close_have_one_winner() {
        let dir = TestDir::new();
        let store = ConversationStore::new(dir.path().join("conversation.jsonl"));
        store.ensure().await.unwrap();
        let inbox = std::sync::Arc::new(
            InputInbox::load(dir.path().join("inputs.json"), PersistenceFault::default()).unwrap(),
        );
        for index in 0..8 {
            let turn = format!("turn-{index}");
            inbox
                .accept(input(&format!("task-{index}"), None), Some(&turn))
                .unwrap();
            inbox.apply_pending(&store, || false).await.unwrap();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let admitting = inbox.clone();
            let start = barrier.clone();
            let guide = std::thread::spawn(move || {
                start.wait();
                admitting.accept(input(&format!("guide-{index}"), Some(&turn)), None)
            });
            barrier.wait();
            let closed = inbox.close_if_empty();
            let accepted = guide.join().unwrap().is_ok();
            assert_ne!(closed, accepted);
            inbox.finish().unwrap();
        }
    }
}
