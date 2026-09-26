//! Sliding format-error window for one user turn.
//!
//! A "round" is one model answer based on the committed history; every round
//! reaches exactly one terminal judgement and appends exactly one slot. A slot
//! is `true` when the round needed format feedback (bad tool arguments,
//! unusable model output) or had response-protocol damage from a retried
//! attempt. Successful and request-only retries append `false` once.
//!
//! The window bounds *future autonomous advancement*; it never rolls back
//! tools that already ran.

use std::collections::VecDeque;

#[derive(Debug, Clone)]
pub(super) struct FormatRecoveryWindow {
    window: VecDeque<bool>,
    size: usize,
    max_errors: usize,
}

impl FormatRecoveryWindow {
    pub(super) fn new(size: usize, max_errors: usize) -> Self {
        debug_assert!(size >= 1, "format window size is validated at startup");
        debug_assert!(
            max_errors < size,
            "format error cap is validated at startup"
        );
        Self {
            window: VecDeque::with_capacity(size),
            size,
            max_errors,
        }
    }

    /// Number of `true` slots currently inside the window.
    pub(super) fn error_count(&self) -> usize {
        self.window.iter().filter(|slot| **slot).count()
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.window.len()
    }

    /// Side-effect-free pre-check: would committing `had_error` exhaust the
    /// budget? The caller must not use this to insert slots.
    pub(super) fn would_exceed(&self, had_error: bool) -> bool {
        if !had_error {
            return self.error_count() > self.max_errors;
        }
        // Prospective window after appending `true` and trimming to size.
        let mut count = self.error_count();
        if self.window.len() == self.size
            && let Some(oldest) = self.window.front()
            && *oldest
        {
            count -= 1;
        }
        count + 1 > self.max_errors
    }

    /// Append exactly one slot for a completed round; returns `true` when the
    /// window is exhausted (the turn must end with format recovery failure).
    pub(super) fn commit(&mut self, had_error: bool) -> bool {
        self.window.push_back(had_error);
        while self.window.len() > self.size {
            self.window.pop_front();
        }
        self.error_count() > self.max_errors
    }

    /// 清空窗口（新用户 turn 通过重新构造窗口清空；子代理独立）。
    #[cfg(test)]
    pub(super) fn clear(&mut self) {
        self.window.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The W=5/K=2 example from the design: E,N,E,N,N, then E, then E.
    #[test]
    fn sliding_example_matches_design_expectations() {
        let mut window = FormatRecoveryWindow::new(5, 2);
        for slot in [true, false, true, false, false] {
            assert!(!window.commit(slot));
        }
        assert_eq!(window.error_count(), 2);
        assert!(!window.commit(true)); // N,E,N,N,E
        assert_eq!(window.error_count(), 2);
        assert!(window.commit(true)); // E,N,N,E,E -> 3 > 2
        assert_eq!(window.error_count(), 3);
    }

    #[test]
    fn precheck_never_mutates_state() {
        let mut window = FormatRecoveryWindow::new(5, 2);
        for slot in [true, false, true, false, false] {
            window.commit(slot);
        }
        let before = window.len();
        assert!(!window.would_exceed(true));
        assert!(!window.would_exceed(true));
        assert_eq!(window.len(), before);
        assert_eq!(window.error_count(), 2);
        window.commit(true);
        assert!(window.would_exceed(true)); // the next error would exhaust
        assert_eq!(window.error_count(), 2);
        assert_eq!(window.len(), 5);
    }

    #[test]
    fn k_zero_exhausts_on_first_error() {
        let mut window = FormatRecoveryWindow::new(1, 0);
        assert!(window.would_exceed(true));
        assert!(window.commit(true));
        assert!(!window.commit(false));
        let mut window = FormatRecoveryWindow::new(3, 0);
        assert!(window.would_exceed(true));
        // A clean round does not consume the (empty) format budget.
        assert!(!window.commit(false));
        // ...but the very next error already exhausts it.
        assert!(window.would_exceed(true));
        assert!(window.commit(true));
    }

    #[test]
    fn long_clean_run_never_accumulates_errors() {
        let mut window = FormatRecoveryWindow::new(10, 3);
        for _ in 0..20 {
            assert!(!window.commit(false));
        }
        assert_eq!(window.error_count(), 0);
        window.clear();
        assert!(!window.would_exceed(true));
    }
}
