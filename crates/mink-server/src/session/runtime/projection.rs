use super::*;

pub(super) struct Projection {
    pub(super) generation: String,
    pub(super) watermark: u64,
    pub(super) rows: Vec<serde_json::Value>,
    /// Serialized row sizes, aligned with `rows`, so trimming does not
    /// re-serialize the whole window on every commit.
    pub(super) row_bytes: Vec<usize>,
    /// Older rows were dropped from the projection; the reader can page them
    /// back through the conversation endpoint.
    pub(super) has_older: bool,
    pub(super) progress: Vec<serde_json::Value>,
    pub(super) current_turn: Option<String>,
    pub(super) phase: &'static str,
    pub(super) last_final: Option<serde_json::Value>,
    pub(super) resources: serde_json::Value,
    pub(super) diagnostics: serde_json::Value,
    pub(super) activity: serde_json::Value,
    /// Goal mirror rendered into snapshots. It lives here, next to the
    /// watermark and the `goal_updated` payload, so a snapshot never has to
    /// reach for the state lock while holding the projection: the lock order
    /// state -> projection stays one-way and cannot invert.
    pub(super) goal: Option<mink_goal::GoalSnapshot>,
}
impl Projection {
    /// Seed a newly opened projection. Only the retained tail is serialized:
    /// computing row sizes for the whole 17 MB conversation just to drop most
    /// of it made opening a long session slower than the snapshot itself.
    pub(super) fn trim_initial(&mut self) {
        let boundaries: Vec<_> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                row["role"] == "user"
                    && row["content"].is_string()
                    && row["internal"] != true
                    && row.pointer("/_mink/guidance") != Some(&serde_json::Value::Bool(true))
            })
            .map(|(i, _)| i)
            .collect();
        if boundaries.len() > SNAPSHOT_RECENT_TURNS {
            let start = boundaries[boundaries.len() - SNAPSHOT_RECENT_TURNS];
            self.rows.drain(..start);
            if start > 0 {
                self.has_older = true;
            }
        }
        self.row_bytes = self
            .rows
            .iter()
            .map(|row| {
                serde_json::to_vec(row)
                    .map(|bytes| bytes.len())
                    .unwrap_or(0)
            })
            .collect();
        self.trim();
    }

    pub(super) fn trim(&mut self) {
        let boundaries: Vec<_> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                row["role"] == "user"
                    && row["content"].is_string()
                    && row["internal"] != true
                    && row.pointer("/_mink/guidance") != Some(&serde_json::Value::Bool(true))
            })
            .map(|(i, _)| i)
            .collect();
        if boundaries.is_empty() {
            return;
        }
        // Start at the oldest of the last N turns; when those still exceed the
        // byte ceiling, move forward one whole turn at a time. The final turn
        // is never dropped, so a tool call/result exchange stays intact.
        let mut first = boundaries.len().saturating_sub(SNAPSHOT_RECENT_TURNS);
        let mut start = if boundaries.len() > SNAPSHOT_RECENT_TURNS {
            boundaries[first]
        } else {
            0
        };
        let bytes_from = |from: usize| self.row_bytes[from..].iter().sum::<usize>();
        while first + 1 < boundaries.len() && bytes_from(start) > SNAPSHOT_MAX_BYTES {
            first += 1;
            start = boundaries[first];
        }
        if start > 0 {
            self.rows.drain(..start);
            self.row_bytes.drain(..start);
            self.has_older = true;
        }
    }
}

impl SessionRuntime {
    pub fn detail(&self) -> serde_json::Value {
        let (generation, current_turn, phase, last_final, goal) = {
            let p = self.projection.lock().unwrap_or_else(|e| e.into_inner());
            (
                p.generation.clone(),
                p.current_turn.clone(),
                p.phase,
                p.last_final.clone(),
                p.goal.clone(),
            )
        };
        serde_json::json!({"generation":generation,"current_turn":current_turn,"phase":phase,"capabilities":{"image_input":self.image_input},"last_final":last_final,"goal":goal.as_ref().filter(|g| !g.hidden).map(goal_summary)})
    }
    pub(crate) fn snapshot_subscription(
        &self,
    ) -> (serde_json::Value, broadcast::Receiver<Arc<BroadcastEvent>>) {
        // Clone the projection under its lock, then serialize outside it: a
        // multi-megabyte snapshot must not hold off the live publishers (and
        // the turn task) while serde walks every row.
        let (
            generation,
            watermark,
            rows,
            has_older,
            progress,
            current_turn,
            phase,
            goal,
            last_final,
            resources,
            diagnostics,
            activity,
            inputs,
            rx,
        ) = {
            let p = self.projection.lock().unwrap_or_else(|e| e.into_inner());
            // Subscribing inside the same critical section is what makes the
            // snapshot watermark and the first delivered event contiguous.
            let rx = self.event_tx.subscribe();
            let inputs = self.inbox.outstanding();
            (
                p.generation.clone(),
                p.watermark,
                p.rows.clone(),
                p.has_older,
                p.progress.clone(),
                p.current_turn.clone(),
                p.phase,
                p.goal.clone(),
                p.last_final.clone(),
                p.resources.clone(),
                p.diagnostics.clone(),
                p.activity.clone(),
                inputs,
                rx,
            )
        };
        (
            serde_json::json!({"type":"session_snapshot","generation":generation,"stream_sequence":watermark,"conversation":rows,"has_older":has_older,"progress":progress,"current_turn":current_turn,"phase":phase,"running":matches!(phase,"running"|"cancelling"|"closing"),"inputs":inputs,"goal":goal.as_ref().filter(|g| !g.hidden).map(goal_summary),"capabilities":{"image_input":self.image_input},"last_final":last_final,"resources":resources,"diagnostics":diagnostics,"activity":activity}),
            rx,
        )
    }
}
