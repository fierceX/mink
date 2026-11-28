//! Hierarchical reversible disposer scopes (`Scope` / `Disposer` / `ScopeGuard`).

pub use mink::runtime::ext::scopes_related;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

/// Validation error returned by [`Scope::try_fork`] when a segment is empty
/// or contains `'/'`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeSegmentError {
    pub segment: String,
}

impl fmt::Display for ScopeSegmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SCOPE_SEGMENT_INVALID: scope segment {:?} must be non-empty and must not contain '/'",
            self.segment
        )
    }
}

impl std::error::Error for ScopeSegmentError {}

/// Single-use reversible cleanup action.
///
/// `Drop` does not execute the closure; ownership is normally handed to a
/// [`Scope`] (or executed explicitly via [`Disposer::dispose`]).
pub struct Disposer(Option<Box<dyn FnOnce() + Send>>);

impl Disposer {
    pub fn new(f: impl FnOnce() + Send + 'static) -> Self {
        Self(Some(Box::new(f)))
    }

    pub fn noop() -> Self {
        Self(None)
    }

    pub fn dispose(mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

impl From<mink::runtime::ext::RegistrationGuard> for Disposer {
    fn from(guard: mink::runtime::ext::RegistrationGuard) -> Self {
        Self::new(move || guard.unregister())
    }
}

struct ScopeEntry {
    label: String,
    disposer: Disposer,
    spent_when: Option<Arc<AtomicBool>>,
}

/// Hierarchical lifecycle container whose state advances monotonically:
/// `Live -> Closing -> Disposed`.
pub struct Scope {
    path: Arc<str>,
    items: Mutex<Vec<ScopeEntry>>,
    children: Mutex<Vec<Weak<Scope>>>,
    closing: AtomicBool,
    disposed: Arc<AtomicBool>,
}

impl Scope {
    /// Create a root scope (`""`).
    pub fn root() -> Arc<Self> {
        Arc::new(Self {
            path: Arc::from(""),
            items: Mutex::new(Vec::new()),
            children: Mutex::new(Vec::new()),
            closing: AtomicBool::new(false),
            disposed: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn is_closing(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
    }

    pub fn is_disposed(&self) -> bool {
        self.disposed.load(Ordering::SeqCst)
    }

    /// Fallibly fork a child scope under `segment`, returning
    /// [`ScopeSegmentError`] when `segment` is empty or contains `'/'`.
    pub fn try_fork(self: &Arc<Self>, segment: &str) -> Result<Arc<Self>, ScopeSegmentError> {
        if segment.is_empty() || segment.contains('/') {
            return Err(ScopeSegmentError {
                segment: segment.to_string(),
            });
        }
        let child_path: Arc<str> = if self.path.is_empty() {
            Arc::from(segment)
        } else {
            Arc::from(format!("{}/{}", self.path, segment))
        };
        let child = Arc::new(Self {
            path: child_path,
            items: Mutex::new(Vec::new()),
            children: Mutex::new(Vec::new()),
            closing: AtomicBool::new(self.is_closing()),
            disposed: Arc::new(AtomicBool::new(false)),
        });
        let child_for_dispose = Arc::clone(&child);
        let spent_when = Arc::clone(&child.disposed);
        let entry = ScopeEntry {
            label: format!("child:{}", child.path),
            disposer: Disposer::new(move || child_for_dispose.dispose()),
            spent_when: Some(spent_when),
        };
        let immediate = {
            let mut children = self
                .children
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            children.retain(|weak| weak.strong_count() > 0);
            children.push(Arc::downgrade(&child));
            drop(children);

            // Re-check `self.is_closing()` after registering into `children` so a
            // concurrent `parent.close()` can never miss this new child.
            if self.is_closing() {
                child.close();
            }

            let mut items = self.items.lock().unwrap_or_else(|error| error.into_inner());
            prune_spent(&mut items);
            if self.disposed.load(Ordering::SeqCst) {
                Some(entry.disposer)
            } else {
                items.push(entry);
                None
            }
        };
        if let Some(disposer) = immediate {
            disposer.dispose();
        }
        Ok(child)
    }

    /// Fork a child scope under `segment`.
    ///
    /// Panics if `segment` is empty or contains `'/'`. Prefer
    /// [`Scope::try_fork`] when `segment` comes from external/untrusted input.
    pub fn fork(self: &Arc<Self>, segment: &str) -> Arc<Self> {
        self.try_fork(segment)
            .expect("scope segment must be non-empty and must not contain '/'")
    }

    /// Atomically attach `disposer` only if this scope is still live (neither
    /// `closing` nor `disposed`). If closing/disposed, executes `disposer`
    /// immediately outside locks and returns `false`.
    pub fn try_push_labeled(&self, label: impl Into<String>, disposer: Disposer) -> bool {
        let label = label.into();
        let immediate = {
            let mut items = self.items.lock().unwrap_or_else(|error| error.into_inner());
            prune_spent(&mut items);
            if self.closing.load(Ordering::SeqCst) || self.disposed.load(Ordering::SeqCst) {
                Some(disposer)
            } else {
                items.push(ScopeEntry {
                    label,
                    disposer,
                    spent_when: None,
                });
                None
            }
        };
        if let Some(disposer) = immediate {
            disposer.dispose();
            return false;
        }
        true
    }

    /// Push a labeled disposer into this scope. If the scope is already
    /// disposed, the disposer runs immediately outside any lock.
    pub fn push_labeled(&self, label: impl Into<String>, disposer: Disposer) {
        let label = label.into();
        let immediate = {
            let mut items = self.items.lock().unwrap_or_else(|error| error.into_inner());
            prune_spent(&mut items);
            if self.disposed.load(Ordering::SeqCst) {
                Some(disposer)
            } else {
                items.push(ScopeEntry {
                    label,
                    disposer,
                    spent_when: None,
                });
                None
            }
        };
        if let Some(disposer) = immediate {
            disposer.dispose();
        }
    }

    /// Recursively mark this scope and all live descendants as `closing`.
    pub fn close(&self) {
        self.closing.store(true, Ordering::SeqCst);
        let live_children: Vec<Arc<Scope>> = {
            let mut children = self
                .children
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            children.retain(|weak| weak.strong_count() > 0);
            children.iter().filter_map(Weak::upgrade).collect()
        };
        for child in live_children {
            child.close();
        }
    }

    /// Idempotently dispose this scope and all attached disposers in strict
    /// LIFO order outside any lock.
    pub fn dispose(&self) {
        self.closing.store(true, Ordering::SeqCst);
        if self.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        let entries = {
            let mut items = self.items.lock().unwrap_or_else(|error| error.into_inner());
            std::mem::take(&mut *items)
        };
        self.children
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        for entry in entries.into_iter().rev() {
            entry.disposer.dispose();
        }
    }

    pub fn live_labels(&self) -> Vec<String> {
        let mut items = self.items.lock().unwrap_or_else(|error| error.into_inner());
        prune_spent(&mut items);
        items.iter().map(|entry| entry.label.clone()).collect()
    }

    pub fn len(&self) -> usize {
        let mut items = self.items.lock().unwrap_or_else(|error| error.into_inner());
        prune_spent(&mut items);
        items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn prune_spent(items: &mut Vec<ScopeEntry>) {
    items.retain(|entry| {
        !entry
            .spent_when
            .as_ref()
            .is_some_and(|spent| spent.load(Ordering::Acquire))
    });
}

/// RAII guard that disposes the wrapped [`Scope`] on drop.
pub struct ScopeGuard(Arc<Scope>);

impl ScopeGuard {
    pub fn new(scope: Arc<Scope>) -> Self {
        Self(scope)
    }

    pub fn scope(&self) -> &Arc<Scope> {
        &self.0
    }
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        self.0.dispose();
    }
}
