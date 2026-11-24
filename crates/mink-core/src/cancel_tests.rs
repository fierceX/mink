use super::*;

#[tokio::test]
async fn cancel_wakes_cancelled() {
    let token = CancellationToken::new();
    assert!(!token.is_cancelled());
    token.cancel();
    assert!(token.is_cancelled());
    token.cancelled().await; // should return immediately
}

#[tokio::test]
async fn linked_child_cancel_does_not_cancel_parent() {
    let parent = CancellationToken::new();
    let child = parent.linked_child_token();
    child.cancel();
    assert!(child.is_cancelled());
    assert!(!parent.is_cancelled());
}

#[tokio::test]
async fn linked_child_observes_parent_cancel() {
    let parent = CancellationToken::new();
    let child = parent.linked_child_token();
    parent.cancel();
    child.cancelled().await;
    assert!(child.is_cancelled());
}

#[tokio::test]
async fn dropped_child_does_not_block_propagation_to_later_children() {
    let parent = CancellationToken::new();
    let dropped = parent.linked_child_token();
    drop(dropped);
    let child = parent.linked_child_token();
    parent.cancel();
    child.cancelled().await;
    assert!(child.is_cancelled());
}

#[tokio::test]
async fn child_linked_after_parent_cancel_starts_cancelled() {
    let parent = CancellationToken::new();
    parent.cancel();
    let child = parent.linked_child_token();
    assert!(child.is_cancelled());
}

#[tokio::test]
async fn interrupt_signal_trigger_before_notified_returns_immediately() {
    let signal = InterruptSignal::new();
    signal.trigger();
    assert!(signal.is_set());
    signal.notified().await;
    signal.reset();
    assert!(!signal.is_set());
}

#[tokio::test]
async fn interrupt_signal_wakes_concurrent_waiters() {
    let signal = InterruptSignal::new();
    let waiter_a = {
        let s = signal.clone();
        tokio::spawn(async move { s.notified().await })
    };
    let waiter_b = {
        let s = signal.clone();
        tokio::spawn(async move { s.notified().await })
    };
    tokio::task::yield_now().await;
    signal.trigger();
    tokio::time::timeout(std::time::Duration::from_secs(1), waiter_a)
        .await
        .expect("waiter A must wake promptly")
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), waiter_b)
        .await
        .expect("waiter B must wake promptly")
        .unwrap();
}

#[tokio::test]
async fn interrupt_signal_trigger_after_reset_wakes_new_waiter() {
    let signal = InterruptSignal::new();
    signal.trigger();
    signal.notified().await;
    signal.reset();

    let cloned = signal.clone();
    let handle = tokio::spawn(async move { cloned.notified().await });
    tokio::task::yield_now().await;
    signal.trigger();
    tokio::time::timeout(std::time::Duration::from_secs(1), handle)
        .await
        .expect("post-reset trigger must wake waiter")
        .unwrap();
}
