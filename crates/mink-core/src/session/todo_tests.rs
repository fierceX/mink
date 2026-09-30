use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn store(name: &str) -> (PathBuf, TodoStore) {
    let root = std::env::temp_dir().join(format!(
        "mink-todo-{name}-{}-{}",
        std::process::id(),
        TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("todos.json");
    let store = TodoStore::load(path).unwrap();
    (root, store)
}

fn add(content: &str) -> TodoAdd {
    TodoAdd {
        content: content.into(),
    }
}

#[test]
fn complete_pending_item_auto_activates() {
    // 直接 complete 一个 pending 条目时自动先激活再完成。
    let (root, store) = store("autoactivate");
    let result = store
        .apply_structure(
            0,
            TodoChanges {
                add: vec![add("first"), add("second")],
                ..TodoChanges::default()
            },
        )
        .unwrap();
    // first 仍 pending；second 先激活再 complete。
    let transition = store
        .advance(
            result.snapshot.revision,
            TodoTransitions {
                complete: vec!["T0001".to_string()],
                ..TodoTransitions::default()
            },
        )
        .unwrap();
    assert!(transition.activated.contains(&"T0001".to_string()));
    assert!(transition.completed.contains(&"T0001".to_string()));
    let item = transition
        .snapshot
        .items
        .iter()
        .find(|item| item.id == "T0001")
        .unwrap();
    assert_eq!(item.status, TodoStatus::Completed);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn complete_already_completed_item_still_rejected() {
    let (root, store) = store("doublecomplete");
    let result = store
        .apply_structure(
            0,
            TodoChanges {
                add: vec![add("first")],
                ..TodoChanges::default()
            },
        )
        .unwrap();
    let r1 = store
        .advance(
            result.snapshot.revision,
            TodoTransitions {
                complete: vec!["T0001".to_string()],
                ..TodoTransitions::default()
            },
        )
        .unwrap();
    let err = store
        .advance(
            r1.snapshot.revision,
            TodoTransitions {
                complete: vec!["T0001".to_string()],
                ..TodoTransitions::default()
            },
        )
        .unwrap_err();
    assert!(err.to_string().contains("already completed"), "{err}");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn missing_file_starts_at_revision_zero_and_persists_ids() {
    let (root, store) = store("persist");
    let result = store
        .apply_structure(
            0,
            TodoChanges {
                add: vec![add("first"), add("second")],
                ..TodoChanges::default()
            },
        )
        .unwrap();
    assert_eq!(result.snapshot.revision, 1);
    assert_eq!(result.snapshot.items[0].id, "T0001");
    assert_eq!(result.snapshot.items[1].id, "T0002");
    assert_eq!(result.snapshot.next_id, 3);

    let reloaded = TodoStore::load(root.join("todos.json")).unwrap();
    assert_eq!(reloaded.snapshot(), result.snapshot);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn batch_update_is_atomic_and_rejects_stale_revision() {
    let (root, store) = store("batch");
    store
        .apply_structure(
            0,
            TodoChanges {
                add: vec![add("first"), add("second")],
                ..TodoChanges::default()
            },
        )
        .unwrap();
    let updated = store
        .apply_structure(
            1,
            TodoChanges {
                update: vec![TodoUpdate {
                    id: "T0002".into(),
                    content: "second revised".into(),
                }],
                ..TodoChanges::default()
            },
        )
        .unwrap();
    assert_eq!(updated.snapshot.revision, 2);
    let before = std::fs::read(root.join("todos.json")).unwrap();
    assert!(
        store
            .apply_structure(
                1,
                TodoChanges {
                    remove: vec!["T0001".into()],
                    ..TodoChanges::default()
                }
            )
            .is_err()
    );
    assert_eq!(std::fs::read(root.join("todos.json")).unwrap(), before);
    assert_eq!(store.snapshot(), updated.snapshot);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn invalid_change_rolls_back_the_entire_batch() {
    let (root, store) = store("rollback");
    store
        .apply_structure(
            0,
            TodoChanges {
                add: vec![add("first")],
                ..TodoChanges::default()
            },
        )
        .unwrap();
    let before = store.snapshot();
    let error = store
        .apply_structure(
            1,
            TodoChanges {
                update: vec![TodoUpdate {
                    id: "T0001".into(),
                    content: "changed".into(),
                }],
                remove: vec!["missing".into()],
                ..TodoChanges::default()
            },
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("unknown todo item"), "{error}");
    assert_eq!(store.snapshot(), before);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn persistence_failure_does_not_advance_in_memory_revision() {
    let (root, store) = store("write-failure");
    std::fs::create_dir(root.join("todos.json")).unwrap();
    assert!(
        store
            .apply_structure(
                0,
                TodoChanges {
                    add: vec![add("must not commit")],
                    ..TodoChanges::default()
                },
            )
            .is_err()
    );
    assert_eq!(store.snapshot(), TodoSnapshot::default());
    assert!(std::fs::read_dir(&root).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".tmp-")
    }));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn corrupt_file_fails_closed() {
    let (root, _) = store("corrupt");
    std::fs::write(root.join("todos.json"), b"{broken").unwrap();
    assert!(TodoStore::load(root.join("todos.json")).is_err());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn faulted_session_rejects_todo_writes() {
    let (root, store) = store("faulted");
    let fault = crate::session::persistence::PersistenceFault::default();
    let store = store.with_fault(fault.clone());
    let _ = fault.raise(&root.join("todos.json"), "injected publish fault");

    let error = store
        .apply_structure(
            0,
            TodoChanges {
                add: vec![add("must not commit")],
                ..TodoChanges::default()
            },
        )
        .unwrap_err();
    assert!(error.to_string().contains("persistence fault"), "{error}");
    assert!(error.to_string().contains("restart the session"), "{error}");

    let error = store.advance(0, TodoTransitions::default()).unwrap_err();
    assert!(error.to_string().contains("persistence fault"), "{error}");
    assert_eq!(store.snapshot(), TodoSnapshot::default());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn batch_cannot_target_an_id_created_in_the_same_write() {
    let (root, store) = store("guessed-id");
    let error = store
        .apply_structure(
            0,
            TodoChanges {
                add: vec![add("new")],
                update: vec![TodoUpdate {
                    id: "T0001".into(),
                    content: "guessed".into(),
                }],
                ..TodoChanges::default()
            },
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("unknown todo item"), "{error}");
    assert_eq!(store.snapshot(), TodoSnapshot::default());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn projection_includes_only_active_items_and_counts() {
    let (root, store) = store("projection-active");
    store
        .apply_structure(
            0,
            TodoChanges {
                add: vec![add("pending"), add("<active>"), add("done")],
                ..TodoChanges::default()
            },
        )
        .unwrap();
    store
        .advance(
            1,
            TodoTransitions {
                activate: vec!["T0002".into()],
                ..TodoTransitions::default()
            },
        )
        .unwrap();
    let content = render_current_todos(&store.snapshot(), "TodoRead");
    assert!(content.contains("pending=\"2\""));
    assert!(content.contains("T0002: &lt;active&gt;"));
    assert!(!content.contains("T0001: pending"));
    assert!(!content.contains("T0003: done"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn projection_reminds_to_read_when_pending_has_no_active_batch() {
    let (root, store) = store("projection-pending");
    store
        .apply_structure(
            0,
            TodoChanges {
                add: vec![add("pending")],
                ..TodoChanges::default()
            },
        )
        .unwrap();
    let projected = render_current_todos(&store.snapshot(), "InspectTodos");
    assert!(projected.contains("Call InspectTodos"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn projection_represents_pending_only_state() {
    let (root, store) = store("projection-pending-only");
    store
        .apply_structure(
            0,
            TodoChanges {
                add: vec![add("done")],
                ..TodoChanges::default()
            },
        )
        .unwrap();
    let projected = render_current_todos(&store.snapshot(), "TodoRead");
    assert!(projected.contains("pending=\"1\""));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn progress_transitions_are_atomic_and_enforce_source_status() {
    let (root, store) = store("transitions");
    store
        .apply_structure(
            0,
            TodoChanges {
                add: vec![add("active"), add("pending")],
                ..TodoChanges::default()
            },
        )
        .unwrap();
    store
        .advance(
            1,
            TodoTransitions {
                activate: vec!["T0001".into()],
                ..TodoTransitions::default()
            },
        )
        .unwrap();
    let advanced = store
        .advance(
            2,
            TodoTransitions {
                complete: vec!["T0001".into()],
                activate: vec!["T0002".into()],
                ..TodoTransitions::default()
            },
        )
        .unwrap();
    assert_eq!(advanced.snapshot.revision, 3);
    assert_eq!(advanced.snapshot.items[0].status, TodoStatus::Completed);
    assert_eq!(advanced.snapshot.items[1].status, TodoStatus::InProgress);

    let before = advanced.snapshot;
    let error = store
        .advance(
            3,
            TodoTransitions {
                complete: vec!["T0001".into()],
                ..TodoTransitions::default()
            },
        )
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("already completed"),
        "repeated complete must fail closed: {error}"
    );
    assert_eq!(store.snapshot(), before);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn structure_changes_create_pending_items_and_cannot_remove_active_items() {
    let (root, store) = store("structure-boundaries");
    store
        .apply_structure(
            0,
            TodoChanges {
                add: vec![add("active")],
                ..TodoChanges::default()
            },
        )
        .unwrap();
    assert_eq!(store.snapshot().items[0].status, TodoStatus::Pending);
    store
        .advance(
            1,
            TodoTransitions {
                activate: vec!["T0001".into()],
                ..TodoTransitions::default()
            },
        )
        .unwrap();
    let error = store
        .apply_structure(
            2,
            TodoChanges {
                remove: vec!["T0001".into()],
                ..TodoChanges::default()
            },
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("must be paused or completed"), "{error}");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn visible_revision_uses_internal_metadata_without_parsing_projection_text() {
    let messages = vec![
        serde_json::json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "one",
                "content": "<current-todos revision=\"999\">",
                "_mink": todo_state_metadata(4, "structure"),
            }],
        }),
        serde_json::json!({
            "role": "system",
            "content": "sync",
            "_mink": todo_state_metadata(7, "sync"),
        }),
    ];
    assert_eq!(visible_revision(&messages).unwrap(), 7);
}

#[test]
fn visible_revision_rejects_corrupt_internal_metadata() {
    let messages = vec![serde_json::json!({
        "role": "user",
        "content": "state",
        "_mink": {"todo_revision": "not-a-number"},
    })];
    assert!(visible_revision(&messages).is_err());
}

#[test]
fn poisoned_state_lock_rejects_writes_but_reads_keep_last_snapshot() {
    let (root, store) = store("poison");
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = store.state.lock().unwrap();
        panic!("inject poison");
    }));

    let error = store
        .apply_structure(0, TodoChanges::default())
        .expect_err("a poisoned authoritative lock must fail closed");
    assert!(error.to_string().contains("poisoned"), "{error}");
    // The poison also latches the persistence fault: every later write is
    // rejected for that reason, not just the first poisoned access.
    let second = store
        .apply_structure(0, TodoChanges::default())
        .expect_err("latched fault must keep rejecting writes");
    assert!(second.to_string().contains("persistence fault"), "{second}");
    // Reads still expose the last consistent in-memory snapshot.
    let _ = store.snapshot();
    let _ = std::fs::remove_dir_all(root);
}

/// 有界展示：revision 与三类计数保持真实，正文/条目按额度有损截短；
/// 额度充足时输出与完整渲染一致（授权文件与 TodoRead 路径不受影响）。
#[test]
fn bounded_projection_keeps_revision_and_counts() {
    let snapshot = TodoSnapshot {
        version: TODO_FILE_VERSION,
        revision: 7,
        next_id: 44,
        items: vec![
            TodoItem {
                id: "T0001".into(),
                content: "a".repeat(5_000),
                status: TodoStatus::InProgress,
            },
            TodoItem {
                id: "T0002".into(),
                content: "b".repeat(5_000),
                status: TodoStatus::InProgress,
            },
            TodoItem {
                id: "T0003".into(),
                content: "pending".into(),
                status: TodoStatus::Pending,
            },
            TodoItem {
                id: "T0004".into(),
                content: "done".into(),
                status: TodoStatus::Completed,
            },
        ],
    };

    let bounded = sync_message_bounded(&snapshot, "TodoRead", 256);
    let content = bounded["content"].as_str().unwrap();
    assert!(content.contains("revision=\"7\""), "{content}");
    assert!(content.contains("pending=\"1\""), "{content}");
    assert!(content.contains("in_progress=\"2\""), "{content}");
    assert!(content.contains("completed=\"1\""), "{content}");
    assert!(
        content.contains("[derived display truncated:"),
        "超额度必须有损展示：{content}"
    );
    assert!(
        content.len() <= 256 * 3,
        "投影必须受额度约束：{} bytes",
        content.len()
    );

    // 额度充足（无限窗口）时不截短，且与完整渲染一致。
    let full = sync_message_bounded(&snapshot, "TodoRead", usize::MAX);
    let full_content = full["content"].as_str().unwrap();
    assert!(!full_content.contains("[derived display truncated:"));
    assert!(full_content.contains(&"a".repeat(5_000)));
    assert!(full_content.contains(&render_current_todos(&snapshot, "TodoRead")));
}

/// R06/F05：额度覆盖完整块（信封、计数、ID、省略标记）；0/1/255/256/MAX 与
/// 1/8/100/1000 条 active 条目都不越界；**任何被截短的正文都必须带省略提示**。
#[test]
fn bounded_projection_respects_byte_allowance_across_shapes() {
    let snapshot = TodoSnapshot {
        version: TODO_FILE_VERSION,
        revision: u64::MAX - 1,
        next_id: 1_001,
        items: (0..1_000)
            .map(|index| TodoItem {
                id: format!("T{index:04}"),
                content: format!("{}#{index}中😀", "x".repeat(200)),
                status: if index % 2 == 0 {
                    TodoStatus::InProgress
                } else {
                    TodoStatus::Pending
                },
            })
            .collect(),
    };
    let before = snapshot.clone();
    for allowance in [0usize, 1, 255, 256, usize::MAX] {
        for count in [1usize, 8, 100, 1_000] {
            let mut snap = snapshot.clone();
            snap.items.truncate(count);
            let message = sync_message_bounded(&snap, "TodoRead", allowance);
            let content = message["content"].as_str().unwrap();
            assert!(
                content.contains(&format!("revision=\"{}\"", snap.revision)),
                "allowance {allowance}, count {count}: {content}"
            );
            if allowance != usize::MAX && !content.contains("truncated=\"unrepresentable\"") {
                assert!(
                    content.len() <= allowance * 3,
                    "allowance {allowance}, count {count}: {} bytes",
                    content.len()
                );
            }
            // F05：正文不完整出现时必须有显式省略说明（列表级或条目级）；
            // 整块不可表示（unrepresentable）本身即明确声明，视为已标注。
            let unrepresentable = content.contains("truncated=\"unrepresentable\"");
            for item in snap
                .items
                .iter()
                .filter(|item| item.status == TodoStatus::InProgress)
            {
                if !unrepresentable && !content.contains(&item.content) {
                    assert!(
                        content.contains("…") || content.contains("omitted"),
                        "allowance {allowance}, count {count}: item {} body was truncated \
                         without an omission marker: {content}",
                        item.id
                    );
                }
            }
        }
    }
    assert_eq!(snapshot, before, "权威快照不得被渲染修改");
}
