//! Incremental, tolerant reader for `events.jsonl`.
//!
//! The writer appends one JSON object per line. A reader may run while the
//! writer is active, so this module only consumes complete lines and leaves a
//! trailing partial line for the next call. Unknown event types and malformed
//! complete lines are counted as skipped instead of failing the whole batch:
//! the event vocabulary is a durable contract that grows over time, and a
//! host reading an older or newer session must still see the records it
//! understands.

use crate::events::{EventLog, SessionEventBatch};
use anyhow::{Result, bail, ensure};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

/// Read at most `max_events` known events starting at `offset`.
///
/// `offset` must be 0 or a value previously returned as `next_offset`. A
/// missing file yields an empty batch; other I/O errors propagate.
pub(crate) fn events_since(
    path: &Path,
    offset: u64,
    max_events: usize,
) -> Result<SessionEventBatch> {
    ensure!(max_events > 0, "max_events must be greater than 0");
    let mut batch = SessionEventBatch {
        next_offset: offset,
        ..SessionEventBatch::default()
    };
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(batch),
        Err(error) => return Err(error.into()),
    };
    let len = file.metadata()?.len();
    if offset > len {
        batch.truncated = true;
        batch.next_offset = 0;
        return Ok(batch);
    }
    if offset > 0 {
        // The caller must pass a line boundary. Rejecting a mid-line offset
        // keeps a truncated first fragment from being parsed as one event.
        file.seek(SeekFrom::Start(offset - 1))?;
        let mut previous = [0u8; 1];
        file.read_exact(&mut previous)?;
        if previous[0] != b'\n' {
            bail!("event log offset {offset} is not at a line boundary");
        }
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = BufReader::new(file);
    let mut consumed = offset;
    let mut line = Vec::new();
    while batch.events.len() < max_events {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        if !line.ends_with(b"\n") {
            // A partial trailing line is not consumed: the writer may still
            // be appending to it.
            break;
        }
        consumed += read as u64;
        let Ok(text) = std::str::from_utf8(&line) else {
            batch.skipped += 1;
            continue;
        };
        let text = text.trim();
        if text.is_empty() {
            batch.skipped += 1;
            continue;
        }
        match serde_json::from_str::<EventLog>(text) {
            Ok(event) => batch.events.push(event),
            Err(_) => batch.skipped += 1,
        }
    }
    batch.next_offset = consumed;
    Ok(batch)
}
