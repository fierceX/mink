//! One capacity ledger for every execution that occupies the server's
//! `max_running` budget: ordinary turns, resumed inputs and goal drivers.
//!
//! The module is deliberately pure: it only owns a map behind one mutex, never
//! calls into a runtime or store, never awaits, and never decides from a
//! status snapshot. A permit is released exactly once, by the owner that
//! received it; an entry identifies the session *instance* (locator plus
//! generation), so a late release from a closed instance cannot remove the
//! entry of a reopened one.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Identity of one running session *instance*: a session reopened later gets a
/// new generation, so permits never leak across close/reopen.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SessionInstanceKey {
    pub(crate) project_key: String,
    pub(crate) id: String,
    pub(crate) generation: u64,
}

/// Why an execution occupies the budget. Kept for diagnostics and for tests
/// that must tell an ordinary turn apart from a goal driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CapacityOwner {
    OrdinaryTurn,
    GoalDriver,
}

/// What the entry is doing: a reservation whose execution has not started yet,
/// one that is running, or one whose execution was asked to stop but has not
/// exited yet. All three occupy capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryPhase {
    Reserved,
    Running,
    Draining,
}

struct Entry {
    id: PermiId,
    owner: CapacityOwner,
    phase: EntryPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PermiId(u64);

impl PermiId {
    pub(crate) fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReserveError {
    /// The pool is at `limit`; nothing may start.
    Full { limit: usize, occupied: usize },
    /// The instance already holds a permit. Callers resolve duplicates by
    /// protocol (idempotent receipt, goal conflict), never by silently sharing
    /// or replacing the existing permit.
    Occupied,
}

struct CapacityState {
    limit: usize,
    next_id: u64,
    entries: HashMap<SessionInstanceKey, Entry>,
}

/// The unique capacity ledger. Cloneable via `Arc` at the call site.
pub(crate) struct CapacityPool {
    inner: Mutex<CapacityState>,
}

impl CapacityPool {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            inner: Mutex::new(CapacityState {
                limit,
                next_id: 1,
                entries: HashMap::new(),
            }),
        }
    }

    /// Reserve one slot atomically. The check and the insert share one lock,
    /// so concurrent callers cannot both observe free capacity.
    pub(crate) fn try_reserve(
        self: &Arc<Self>,
        instance: SessionInstanceKey,
        owner: CapacityOwner,
    ) -> Result<ExecutionPermit, ReserveError> {
        let mut state = self.lock();
        if state.entries.contains_key(&instance) {
            return Err(ReserveError::Occupied);
        }
        if state.entries.len() >= state.limit {
            return Err(ReserveError::Full {
                limit: state.limit,
                occupied: state.entries.len(),
            });
        }
        let id = PermiId(state.next_id);
        state.next_id = state.next_id.saturating_add(1);
        state.entries.insert(
            instance.clone(),
            Entry {
                id,
                owner,
                phase: EntryPhase::Reserved,
            },
        );
        Ok(ExecutionPermit {
            pool: self.clone(),
            instance,
            id,
            released: false,
        })
    }

    /// Mark the entry of one session instance as draining.
    ///
    /// A stop request names the instance rather than a permit: only an
    /// occupied entry (Reserved or Running) becomes Draining, so a request
    /// racing an exit cannot resurrect or clear anything.
    pub(crate) fn mark_draining(&self, instance: &SessionInstanceKey) -> bool {
        let mut state = self.lock();
        match state.entries.get_mut(instance) {
            Some(entry) if entry.phase != EntryPhase::Draining => {
                entry.phase = EntryPhase::Draining;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn occupied(&self) -> usize {
        self.lock().entries.len()
    }

    pub(crate) fn limit(&self) -> usize {
        self.lock().limit
    }

    pub(crate) fn phase_of(&self, instance: &SessionInstanceKey) -> Option<EntryPhase> {
        self.lock().entries.get(instance).map(|entry| entry.phase)
    }

    pub(crate) fn owner_of(&self, instance: &SessionInstanceKey) -> Option<CapacityOwner> {
        self.lock().entries.get(instance).map(|entry| entry.owner)
    }

    /// Remove the entry only when the release names the current owner. A stale
    /// permit (closed instance, replaced execution) is a no-op.
    pub(crate) fn release(&self, instance: &SessionInstanceKey, id: PermiId) {
        let mut state = self.lock();
        if state
            .entries
            .get(instance)
            .is_some_and(|entry| entry.id == id)
        {
            state.entries.remove(instance);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CapacityState> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// One execution's claim on capacity. Dropping it releases the claim; no other
/// code path may remove the entry.
pub(crate) struct ExecutionPermit {
    pool: Arc<CapacityPool>,
    instance: SessionInstanceKey,
    id: PermiId,
    released: bool,
}

impl ExecutionPermit {
    pub(crate) fn id(&self) -> PermiId {
        self.id
    }

    pub(crate) fn instance(&self) -> &SessionInstanceKey {
        &self.instance
    }

    /// The execution actually started. Reserved -> Running.
    pub(crate) fn mark_running(&self) -> bool {
        self.transition(EntryPhase::Reserved, EntryPhase::Running)
    }

    /// A stop was requested but the execution has not exited yet.
    pub(crate) fn mark_draining(&self) -> bool {
        self.transition(EntryPhase::Running, EntryPhase::Draining)
    }

    fn transition(&self, from: EntryPhase, to: EntryPhase) -> bool {
        let mut state = self.pool.lock();
        match state.entries.get_mut(&self.instance) {
            Some(entry) if entry.id == self.id && entry.phase == from => {
                entry.phase = to;
                true
            }
            _ => false,
        }
    }
}

impl Drop for ExecutionPermit {
    fn drop(&mut self) {
        if !self.released {
            self.released = true;
            self.pool.release(&self.instance, self.id);
        }
    }
}

/// The host side of `mink::runtime::GoalRunLease`: a pool permit that stays
/// alive for exactly as long as the goal run core keeps the lease.
pub(crate) struct PermitLease {
    permit: ExecutionPermit,
}

impl PermitLease {
    pub(crate) fn new(permit: ExecutionPermit) -> Self {
        Self { permit }
    }

    /// The execution actually started; the run keeps occupying capacity.
    pub(crate) fn mark_running(&self) -> bool {
        self.permit.mark_running()
    }

    pub(crate) fn mark_draining(&self) -> bool {
        self.permit.mark_draining()
    }
}

impl mink::runtime::GoalRunLease for PermitLease {}

impl Drop for PermitLease {
    fn drop(&mut self) {
        // The permit's own Drop releases the entry; this is only here to make
        // the capacity lifetime explicit at the lease boundary.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    fn instance(id: &str, generation: u64) -> SessionInstanceKey {
        SessionInstanceKey {
            project_key: "proj".into(),
            id: id.into(),
            generation,
        }
    }

    #[test]
    fn reserve_release_and_occupancy_are_exact() {
        let pool = Arc::new(CapacityPool::new(2));
        let permit = pool
            .try_reserve(instance("a", 1), CapacityOwner::OrdinaryTurn)
            .unwrap();
        assert_eq!(pool.occupied(), 1);
        assert_eq!(
            pool.owner_of(&instance("a", 1)),
            Some(CapacityOwner::OrdinaryTurn)
        );
        // A second reservation of the same instance is refused while it holds
        // one: sharing would release someone else's slot on drop.
        assert!(matches!(
            pool.try_reserve(instance("a", 1), CapacityOwner::OrdinaryTurn),
            Err(ReserveError::Occupied)
        ));
        drop(permit);
        assert_eq!(pool.occupied(), 0);
        assert_eq!(pool.phase_of(&instance("a", 1)), None);
    }

    #[test]
    fn concurrent_reservations_never_exceed_the_limit() {
        let pool = Arc::new(CapacityPool::new(3));
        let barrier = Arc::new(Barrier::new(16));
        let mut handles = Vec::new();
        for n in 0..16 {
            let pool = pool.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                pool.try_reserve(instance(&format!("s{n}"), 1), CapacityOwner::OrdinaryTurn)
            }));
        }
        // Permits stay alive in the results, so the pool must hold exactly the
        // limit while every other attempt is refused.
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        let granted = results.iter().filter(|result| result.is_ok()).count();
        assert_eq!(granted, 3);
        assert_eq!(pool.occupied(), 3);
    }

    #[test]
    fn full_pool_reports_the_limit() {
        let pool = Arc::new(CapacityPool::new(1));
        let _first = pool
            .try_reserve(instance("a", 1), CapacityOwner::OrdinaryTurn)
            .unwrap();
        assert!(matches!(
            pool.try_reserve(instance("b", 1), CapacityOwner::GoalDriver),
            Err(ReserveError::Full {
                limit: 1,
                occupied: 1
            })
        ));
    }

    #[test]
    fn stop_requests_mark_the_entry_draining() {
        let pool = Arc::new(CapacityPool::new(1));
        let permit = pool
            .try_reserve(instance("a", 1), CapacityOwner::GoalDriver)
            .unwrap();
        assert!(permit.mark_running());
        assert!(pool.mark_draining(&instance("a", 1)));
        assert_eq!(pool.phase_of(&instance("a", 1)), Some(EntryPhase::Draining));
        // Draining still occupies capacity, and a repeated request is a no-op.
        assert!(!pool.mark_draining(&instance("a", 1)));
        assert!(matches!(
            pool.try_reserve(instance("b", 1), CapacityOwner::OrdinaryTurn),
            Err(ReserveError::Full { .. })
        ));
        // A stop request for an empty instance changes nothing.
        assert!(!pool.mark_draining(&instance("c", 1)));
        drop(permit);
        assert_eq!(pool.occupied(), 0);
    }

    #[test]
    fn stale_release_cannot_remove_another_execution() {
        let pool = Arc::new(CapacityPool::new(2));
        let permit = pool
            .try_reserve(instance("a", 1), CapacityOwner::OrdinaryTurn)
            .unwrap();
        let stale_id = permit.id();
        // The instance reopens with a new generation; its entry is separate.
        let reopened = pool
            .try_reserve(instance("a", 2), CapacityOwner::GoalDriver)
            .unwrap();
        // A late release for the old generation cannot touch the new entry.
        assert_eq!(pool.occupied(), 2);
        drop(permit);
        assert_eq!(pool.phase_of(&instance("a", 2)), Some(EntryPhase::Reserved));
        assert_eq!(pool.occupied(), 1);
        // And an old id can never remove the current entry of its own instance.
        let mut state = pool.lock();
        state.entries.get_mut(&instance("a", 2)).unwrap().id = PermiId(stale_id.get() + 1);
        drop(state);
        pool.release(&instance("a", 2), stale_id);
        assert_eq!(
            pool.occupied(),
            1,
            "stale id must not release a newer entry"
        );
        drop(reopened);
        assert_eq!(pool.occupied(), 0);
    }

    #[test]
    fn permit_lease_releases_exactly_when_dropped() {
        let pool = Arc::new(CapacityPool::new(1));
        let permit = pool
            .try_reserve(instance("a", 1), CapacityOwner::GoalDriver)
            .unwrap();
        let lease = PermitLease::new(permit);
        assert_eq!(pool.occupied(), 1);
        assert!(lease.mark_running());
        // The pool stays full while the goal run holds the lease.
        assert!(matches!(
            pool.try_reserve(instance("b", 1), CapacityOwner::OrdinaryTurn),
            Err(ReserveError::Full { .. })
        ));
        drop(lease);
        assert_eq!(
            pool.occupied(),
            0,
            "dropping the lease must release the host capacity"
        );
    }

    #[test]
    fn phases_follow_the_owning_permit_and_occupy_capacity() {
        let pool = Arc::new(CapacityPool::new(1));
        let permit = pool
            .try_reserve(instance("a", 1), CapacityOwner::GoalDriver)
            .unwrap();
        assert!(permit.mark_running());
        assert_eq!(pool.phase_of(&instance("a", 1)), Some(EntryPhase::Running));
        assert!(permit.mark_draining());
        assert_eq!(pool.phase_of(&instance("a", 1)), Some(EntryPhase::Draining));
        // Draining still occupies capacity until the execution exits.
        assert!(matches!(
            pool.try_reserve(instance("b", 1), CapacityOwner::OrdinaryTurn),
            Err(ReserveError::Full { .. })
        ));
        // Transitions are permit-guarded: the same permit cannot replay them.
        assert!(!permit.mark_running());
        assert!(!permit.mark_draining());
        drop(permit);
        assert_eq!(pool.occupied(), 0);
    }
}
