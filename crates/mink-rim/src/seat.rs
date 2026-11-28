//! Explicit three-state single-slot provider arbitration (`SeatRegistry<P>`).

use crate::scope::{Disposer, Scope};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeatError {
    UnknownProvider {
        seat: &'static str,
        configured: String,
        known: Vec<String>,
    },
    Unavailable {
        seat: &'static str,
    },
    Ambiguous {
        seat: &'static str,
        candidates: Vec<String>,
    },
}

impl fmt::Display for SeatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownProvider {
                seat,
                configured,
                known,
            } => write!(
                f,
                "SEAT_PROVIDER_UNKNOWN: seat `{seat}` has no provider `{configured}` (known: {known:?})"
            ),
            Self::Unavailable { seat } => {
                write!(
                    f,
                    "SEAT_PROVIDER_UNAVAILABLE: seat `{seat}` has no available provider"
                )
            }
            Self::Ambiguous { seat, candidates } => write!(
                f,
                "SEAT_PROVIDER_AMBIGUOUS: seat `{seat}` has multiple available providers ({candidates:?})"
            ),
        }
    }
}

impl std::error::Error for SeatError {}

type AvailabilityProbe<P> = Arc<dyn Fn(&P) -> bool + Send + Sync>;

struct SeatEntry<P: ?Sized> {
    token: u64,
    id: String,
    provider: Arc<P>,
    available: AvailabilityProbe<P>,
}

/// Single-slot capability registry enforcing explicit selection or unambiguous
/// single-provider arbitration with token-guarded removal.
pub struct SeatRegistry<P: ?Sized + Send + Sync + 'static> {
    seat: &'static str,
    entries: Arc<RwLock<Vec<SeatEntry<P>>>>,
    next_token: AtomicU64,
}

impl<P: ?Sized + Send + Sync + 'static> SeatRegistry<P> {
    pub fn new(seat: &'static str) -> Self {
        Self {
            seat,
            entries: Arc::new(RwLock::new(Vec::new())),
            next_token: AtomicU64::new(1),
        }
    }

    pub fn seat(&self) -> &'static str {
        self.seat
    }

    /// Register a provider under `id`, returning a token-guarded [`Disposer`].
    /// Re-registering an existing `id` replaces the prior slot while ensuring
    /// stale disposers cannot remove newer registrations.
    pub fn register(
        &self,
        id: impl Into<String>,
        provider: Arc<P>,
        available: impl Fn(&P) -> bool + Send + Sync + 'static,
    ) -> Disposer {
        let id = id.into();
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        {
            let mut guard = self
                .entries
                .write()
                .unwrap_or_else(|error| error.into_inner());
            guard.retain(|entry| entry.id != id);
            guard.push(SeatEntry {
                token,
                id,
                provider,
                available: Arc::new(available),
            });
        }
        let entries = Arc::clone(&self.entries);
        Disposer::new(move || {
            let mut guard = entries.write().unwrap_or_else(|error| error.into_inner());
            guard.retain(|entry| entry.token != token);
        })
    }

    /// Register a provider and attach its disposer directly to `scope`.
    pub fn register_in(
        &self,
        scope: &Scope,
        id: impl Into<String>,
        provider: Arc<P>,
        available: impl Fn(&P) -> bool + Send + Sync + 'static,
    ) {
        let id = id.into();
        let label = format!("seat:{}:{id}", self.seat);
        let disposer = self.register(id, provider, available);
        scope.push_labeled(label, disposer);
    }

    /// Resolve a provider deterministically:
    /// 1. Explicit `configured` ID -> must exist (`UnknownProvider`) and be
    ///    available (`Unavailable`), never silently falling back.
    /// 2. Unconfigured -> exactly 1 available provider succeeds; 0 ->
    ///    `Unavailable`; >1 -> `Ambiguous`.
    pub fn resolve(&self, configured: Option<&str>) -> Result<Arc<P>, SeatError> {
        let snapshot: Vec<(String, Arc<P>, AvailabilityProbe<P>)> = {
            let guard = self
                .entries
                .read()
                .unwrap_or_else(|error| error.into_inner());
            guard
                .iter()
                .map(|entry| {
                    (
                        entry.id.clone(),
                        Arc::clone(&entry.provider),
                        Arc::clone(&entry.available),
                    )
                })
                .collect()
        };

        if let Some(target_id) = configured {
            if let Some((_, provider, available)) =
                snapshot.iter().find(|(id, _, _)| id == target_id)
            {
                if available(provider) {
                    return Ok(Arc::clone(provider));
                }
                return Err(SeatError::Unavailable { seat: self.seat });
            }
            return Err(SeatError::UnknownProvider {
                seat: self.seat,
                configured: target_id.to_string(),
                known: snapshot.into_iter().map(|(id, _, _)| id).collect(),
            });
        }

        let mut available_candidates = Vec::new();
        for (id, provider, available) in snapshot {
            if available(&provider) {
                available_candidates.push((id, provider));
            }
        }
        match available_candidates.len() {
            0 => Err(SeatError::Unavailable { seat: self.seat }),
            1 => Ok(available_candidates.into_iter().next().expect("one").1),
            _ => Err(SeatError::Ambiguous {
                seat: self.seat,
                candidates: available_candidates.into_iter().map(|(id, _)| id).collect(),
            }),
        }
    }

    pub fn known_ids(&self) -> Vec<String> {
        self.entries
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .map(|entry| entry.id.clone())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.entries
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
