//! Typed NDJSON wire protocol, lossless JSON validation, bounded output ledger,
//! and shared host-side control loop used by both Subprocess and WASI PTC runtimes.

use super::PtcConfig;
use super::runtime::{
    PtcBindingDispatcher, PtcBindingError, PtcBindingSuccess, PtcFailureKind, PtcImageDescriptor,
    PtcOutputItem, PtcRunFailure, PtcRunRequest, PtcRunResult, PtcRunSpec,
};
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

pub const PTC_PROTOCOL_VERSION: u32 = 1;
pub const MAX_JSON_DEPTH: usize = 64;

/// Messages sent from the Rust host to the Python guest over the dedicated control channel.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessage {
    Run {
        code: String,
        cwd: String,
        tool_names: Vec<String>,
        max_output_bytes: usize,
    },
    ToolReply {
        id: u64,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        value: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        inspection: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        image: Option<PtcImageDescriptor>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<WireToolError>,
    },
    Revoke {
        reason: String,
    },
    DiscardOutput,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireToolError {
    pub tool_name: String,
    pub kind: String,
    pub message: String,
}

/// Messages received from the Python guest over the dedicated control channel.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GuestMessage {
    Ready {
        protocol_version: u32,
    },
    ToolCall {
        id: u64,
        name: String,
        args: Value,
    },
    Output {
        kind: String,
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        image_id: Option<String>,
    },
    Done {
        ok: bool,
        #[serde(default)]
        error: Option<WireProgramError>,
        #[serde(default)]
        output_truncated: bool,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireProgramError {
    pub kind: String,
    pub message: String,
}

impl WireProgramError {
    pub fn into_failure(self) -> PtcRunFailure {
        let kind = match self.kind.as_str() {
            "timeout" => PtcFailureKind::Timeout,
            "abort" => PtcFailureKind::Abort,
            "protocol" => PtcFailureKind::Protocol,
            "sandbox_unavailable" => PtcFailureKind::SandboxUnavailable,
            "worker_exit" => PtcFailureKind::WorkerExit,
            _ => PtcFailureKind::Exception,
        };
        PtcRunFailure {
            kind,
            message: self.message,
        }
    }
}

/// Iteratively validate that a JSON value stays within depth bounds and contains
/// no malformed structures.
pub fn validate_lossless_json(value: &Value, max_depth: usize) -> Result<()> {
    let mut stack: Vec<(&Value, usize)> = vec![(value, 0)];
    while let Some((node, depth)) = stack.pop() {
        ensure!(
            depth <= max_depth,
            "JSON value exceeds maximum nesting depth of {max_depth}"
        );
        match node {
            Value::Null | Value::Bool(_) | Value::String(_) => {}
            Value::Number(num) => {
                if let Some(f) = num.as_f64() {
                    ensure!(f.is_finite(), "non-finite JSON number is not allowed");
                }
            }
            Value::Array(items) => {
                for item in items {
                    stack.push((item, depth + 1));
                }
            }
            Value::Object(map) => {
                for val in map.values() {
                    stack.push((val, depth + 1));
                }
            }
        }
    }
    Ok(())
}

/// Decode one untrusted control line from the guest while enforcing a hard byte cap.
pub fn decode_guest_line(line: &[u8], max_message_bytes: usize) -> Result<GuestMessage> {
    if line.len() > max_message_bytes {
        bail!(
            "guest control message exceeds limit ({} > {} bytes)",
            line.len(),
            max_message_bytes
        );
    }
    let msg: GuestMessage =
        serde_json::from_slice(line).map_err(|e| anyhow::anyhow!("invalid control JSON: {e}"))?;
    if let GuestMessage::ToolCall { args, .. } = &msg {
        validate_lossless_json(args, MAX_JSON_DEPTH)?;
    }
    Ok(msg)
}

/// Bounded ledger accounting for captured program text and image emissions.
pub struct OutputLedger {
    max_bytes: usize,
    used_bytes: usize,
    pub truncated: bool,
    pub items: Vec<PtcOutputItem>,
}

const IMAGE_ENTRY_OVERHEAD_BYTES: usize = 128;

impl OutputLedger {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            used_bytes: 0,
            truncated: false,
            items: Vec::new(),
        }
    }

    /// Admit text into the ledger, preserving adjacent order and setting `truncated`
    /// once the configured byte budget is reached. Returns `true` if newly truncated.
    pub fn push_text(&mut self, text: &str) -> bool {
        if self.truncated || text.is_empty() {
            return false;
        }
        let remaining = self.max_bytes.saturating_sub(self.used_bytes);
        if remaining == 0 {
            self.truncated = true;
            return true;
        }
        if text.len() <= remaining {
            self.used_bytes += text.len();
            self.append_text_item(text);
            false
        } else {
            let mut cut = remaining;
            while cut > 0 && !text.is_char_boundary(cut) {
                cut -= 1;
            }
            if cut > 0 {
                self.used_bytes += cut;
                self.append_text_item(&text[..cut]);
            }
            self.truncated = true;
            true
        }
    }

    /// Admit an inspected image entry into the ledger. Returns `true` if newly truncated.
    pub fn push_image(&mut self, image_id: String) -> bool {
        if self.truncated {
            return false;
        }
        let cost = IMAGE_ENTRY_OVERHEAD_BYTES.saturating_add(image_id.len());
        if self.used_bytes.saturating_add(cost) > self.max_bytes {
            self.truncated = true;
            return true;
        }
        self.used_bytes += cost;
        self.items.push(PtcOutputItem::Image { image_id });
        false
    }

    fn append_text_item(&mut self, text: &str) {
        if let Some(PtcOutputItem::Text(existing)) = self.items.last_mut() {
            existing.push_str(text);
        } else {
            self.items.push(PtcOutputItem::Text(text.to_string()));
        }
    }

    pub fn take_items(&mut self) -> Vec<PtcOutputItem> {
        std::mem::take(&mut self.items)
    }
}

pub(crate) struct ControlLoopOutcome {
    pub guest_done: bool,
    pub ledger: OutputLedger,
    pub output_truncated: bool,
    pub terminal_error: Option<PtcRunFailure>,
}

impl ControlLoopOutcome {
    pub(crate) fn drain_aux_stream(&mut self, rx: &mut mpsc::Receiver<String>) {
        while let Ok(chunk) = rx.try_recv() {
            if self.ledger.push_text(&chunk) {
                self.output_truncated = true;
            }
        }
    }
}

/// Shared NDJSON host-guest control loop parameterized by reader/writer streams
/// and optional auxiliary stdout/stderr pipe receiver.
pub(crate) async fn run_control_loop<R, W>(
    config: &PtcConfig,
    spec: &PtcRunSpec,
    dispatcher: &Arc<dyn PtcBindingDispatcher>,
    mut ctrl_reader: R,
    mut ctrl_write: W,
    mut aux_stream_rx: Option<&mut mpsc::Receiver<String>>,
) -> ControlLoopOutcome
where
    R: AsyncBufReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let (reply_tx, mut reply_rx) =
        mpsc::unbounded_channel::<(u64, std::result::Result<PtcBindingSuccess, PtcBindingError>)>();

    let allowed_tools: HashSet<String> = spec.tool_names.iter().cloned().collect();
    let mut ledger = OutputLedger::new(config.max_output_bytes);
    let mut line_buf = Vec::new();
    let mut ready_seen = false;
    let mut in_flight_calls = 0usize;
    let mut terminal_error: Option<PtcRunFailure> = None;
    let mut guest_done_truncated = false;
    let mut guest_done = false;
    let mut discard_sent = false;
    let mut revoked = false;
    let mut revoke_deadline: Option<tokio::time::Instant> = None;
    let deadline = tokio::time::Instant::now() + spec.timeout;

    loop {
        let active_deadline = revoke_deadline.unwrap_or(deadline);
        let mut ctrl_closed = false;

        tokio::select! {
            _ = tokio::time::sleep_until(active_deadline) => {
                if revoke_deadline.is_some() {
                    break;
                }
                let msg = format!("PTC execution timed out after {} seconds", spec.timeout.as_secs());
                terminal_error.get_or_insert(PtcRunFailure {
                    kind: PtcFailureKind::Timeout,
                    message: msg.clone(),
                });
                let _ = send_host_msg(&mut ctrl_write, &HostMessage::Revoke { reason: msg }).await;
                revoked = true;
                revoke_deadline = Some(tokio::time::Instant::now() + Duration::from_millis(config.stop_grace_ms));
            }
            _ = spec.cancel.cancelled(), if !revoked => {
                let msg = "PTC execution cancelled".to_string();
                terminal_error.get_or_insert(PtcRunFailure {
                    kind: PtcFailureKind::Abort,
                    message: msg.clone(),
                });
                let _ = send_host_msg(&mut ctrl_write, &HostMessage::Revoke { reason: msg }).await;
                revoked = true;
                revoke_deadline = Some(tokio::time::Instant::now() + Duration::from_millis(config.stop_grace_ms));
            }
            _ = spec.interrupt.notified(), if !revoked => {
                let msg = "PTC execution interrupted".to_string();
                terminal_error.get_or_insert(PtcRunFailure {
                    kind: PtcFailureKind::Abort,
                    message: msg.clone(),
                });
                let _ = send_host_msg(&mut ctrl_write, &HostMessage::Revoke { reason: msg }).await;
                revoked = true;
                revoke_deadline = Some(tokio::time::Instant::now() + Duration::from_millis(config.stop_grace_ms));
            }
            Some((call_id, outcome)) = reply_rx.recv() => {
                in_flight_calls = in_flight_calls.saturating_sub(1);
                if !revoked {
                    let reply = match outcome {
                        Ok(success) => HostMessage::ToolReply {
                            id: call_id,
                            ok: true,
                            value: Some(success.value),
                            inspection: success.inspection_text,
                            image: success.image,
                            error: None,
                        },
                        Err(err) => HostMessage::ToolReply {
                            id: call_id,
                            ok: false,
                            value: None,
                            inspection: None,
                            image: None,
                            error: Some(WireToolError {
                                tool_name: err.tool_name,
                                kind: err.kind.label().to_string(),
                                message: err.message,
                            }),
                        },
                    };
                    if let Err(error) = send_host_msg(&mut ctrl_write, &reply).await {
                        terminal_error.get_or_insert(PtcRunFailure {
                            kind: PtcFailureKind::WorkerExit,
                            message: format!("failed to write tool reply to PTC worker: {error}"),
                        });
                        break;
                    }
                }
            }
            chunk_opt = recv_aux_stream(&mut aux_stream_rx), if aux_stream_rx.is_some() => {
                match chunk_opt {
                    Some(chunk) => {
                        if ledger.push_text(&chunk) && !discard_sent && !revoked {
                            discard_sent = true;
                            let _ = send_host_msg(&mut ctrl_write, &HostMessage::DiscardOutput).await;
                        }
                    }
                    None => {
                        aux_stream_rx = None;
                    }
                }
            }
            read_res = read_bounded_line(&mut ctrl_reader, &mut line_buf, config.max_message_bytes) => {
                match read_res {
                    Ok(0) => {
                        ctrl_closed = true;
                    }
                    Ok(_) => {
                        let trimmed = trim_line_end(&line_buf);
                        if trimmed.is_empty() {
                            continue;
                        }
                        match decode_guest_line(trimmed, config.max_message_bytes) {
                            Ok(GuestMessage::Ready { protocol_version }) => {
                                if protocol_version != PTC_PROTOCOL_VERSION {
                                    terminal_error.get_or_insert(PtcRunFailure {
                                        kind: PtcFailureKind::Protocol,
                                        message: format!("protocol version mismatch: {protocol_version}"),
                                    });
                                    break;
                                }
                                ready_seen = true;
                                let run_msg = HostMessage::Run {
                                    code: spec.code.clone(),
                                    cwd: spec.cwd.display().to_string(),
                                    tool_names: spec.tool_names.clone(),
                                    max_output_bytes: config.max_output_bytes,
                                };
                                if let Err(error) = send_host_msg(&mut ctrl_write, &run_msg).await {
                                    terminal_error.get_or_insert(PtcRunFailure {
                                        kind: PtcFailureKind::WorkerExit,
                                        message: format!("failed to send program to PTC worker: {error}"),
                                    });
                                    break;
                                }
                            }
                            Ok(GuestMessage::ToolCall { id, name, args }) => {
                                if !ready_seen {
                                    terminal_error.get_or_insert(PtcRunFailure {
                                        kind: PtcFailureKind::Protocol,
                                        message: "tool_call received before ready handshake".to_string(),
                                    });
                                    break;
                                }
                                if revoked {
                                    continue;
                                }
                                if !allowed_tools.contains(&name) {
                                    let reply = HostMessage::ToolReply {
                                        id,
                                        ok: false,
                                        value: None,
                                        inspection: None,
                                        image: None,
                                        error: Some(WireToolError {
                                            tool_name: name.clone(),
                                            kind: "SafetyBlocked".to_string(),
                                            message: format!("tool '{name}' is not available in this PTC session"),
                                        }),
                                    };
                                    let _ = send_host_msg(&mut ctrl_write, &reply).await;
                                    continue;
                                }
                                if in_flight_calls >= config.max_pending_calls {
                                    let reason = format!(
                                        "pending PTC tool calls exceeded limit ({})",
                                        config.max_pending_calls
                                    );
                                    terminal_error.get_or_insert(PtcRunFailure {
                                        kind: PtcFailureKind::Protocol,
                                        message: reason.clone(),
                                    });
                                    let _ = send_host_msg(&mut ctrl_write, &HostMessage::Revoke { reason }).await;
                                    revoked = true;
                                    revoke_deadline = Some(tokio::time::Instant::now() + Duration::from_millis(config.stop_grace_ms));
                                    continue;
                                }
                                in_flight_calls += 1;
                                let disp = dispatcher.clone();
                                let tx = reply_tx.clone();
                                tokio::spawn(async move {
                                    let res = disp.invoke(&name, args).await;
                                    let _ = tx.send((id, res));
                                });
                            }
                            Ok(GuestMessage::Output { kind, text, image_id }) => {
                                let newly_truncated = match kind.as_str() {
                                    "text" => text.as_deref().is_some_and(|t| ledger.push_text(t)),
                                    "image" => image_id.is_some_and(|id| ledger.push_image(id)),
                                    other => {
                                        terminal_error.get_or_insert(PtcRunFailure {
                                            kind: PtcFailureKind::Protocol,
                                            message: format!("unknown guest output kind {other:?}"),
                                        });
                                        break;
                                    }
                                };
                                if newly_truncated && !discard_sent && !revoked {
                                    discard_sent = true;
                                    let _ = send_host_msg(&mut ctrl_write, &HostMessage::DiscardOutput).await;
                                }
                            }
                            Ok(GuestMessage::Done { ok, error, output_truncated }) => {
                                guest_done = true;
                                guest_done_truncated = output_truncated;
                                if !ok && terminal_error.is_none() {
                                    terminal_error = Some(error.map_or_else(
                                        || PtcRunFailure {
                                            kind: PtcFailureKind::Exception,
                                            message: "PTC program failed".to_string(),
                                        },
                                        |e| e.into_failure(),
                                    ));
                                }
                                break;
                            }
                            Err(error) => {
                                terminal_error.get_or_insert(PtcRunFailure {
                                    kind: PtcFailureKind::Protocol,
                                    message: error.to_string(),
                                });
                                break;
                            }
                        }
                    }
                    Err(error) => {
                        terminal_error.get_or_insert(PtcRunFailure {
                            kind: PtcFailureKind::Protocol,
                            message: error.to_string(),
                        });
                        break;
                    }
                }
            }
        }
        if ctrl_closed {
            break;
        }
    }

    let output_truncated = ledger.truncated || guest_done_truncated;
    ControlLoopOutcome {
        guest_done,
        ledger,
        output_truncated,
        terminal_error,
    }
}

pub(crate) fn resolve_timeout_and_cwd(
    config: &PtcConfig,
    request: PtcRunRequest,
) -> Result<PtcRunSpec> {
    let timeout_secs = match request.timeout_secs {
        Some(secs) if secs > 0 => {
            if secs > config.max_timeout_secs {
                bail!(
                    "Error: timeout must not exceed {} seconds; got {secs}",
                    config.max_timeout_secs
                );
            }
            secs
        }
        _ => config.timeout_secs.clamp(5, config.max_timeout_secs),
    };
    let cwd = if request.cwd.is_absolute() {
        request.cwd
    } else {
        std::env::current_dir()?.join(request.cwd)
    };
    Ok(PtcRunSpec {
        code: request.code,
        cwd,
        tool_names: request.tool_names,
        timeout: Duration::from_secs(timeout_secs),
        cancel: request.cancel,
        interrupt: request.interrupt,
        sandbox: request.sandbox,
    })
}

pub(crate) fn finalize_run_result(
    mut outcome: ControlLoopOutcome,
    missing_done_message: String,
) -> PtcRunResult {
    if !outcome.guest_done && outcome.terminal_error.is_none() {
        outcome.terminal_error = Some(PtcRunFailure {
            kind: PtcFailureKind::WorkerExit,
            message: missing_done_message,
        });
    }
    PtcRunResult {
        output: outcome.ledger.take_items(),
        output_truncated: outcome.output_truncated,
        error: outcome.terminal_error,
    }
}

async fn recv_aux_stream(rx: &mut Option<&mut mpsc::Receiver<String>>) -> Option<String> {
    match rx {
        Some(inner) => inner.recv().await,
        None => std::future::pending().await,
    }
}

async fn send_host_msg<W: AsyncWriteExt + Unpin>(writer: &mut W, msg: &HostMessage) -> Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_bounded_line<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max_bytes: usize,
) -> Result<usize> {
    buf.clear();
    let mut total = 0usize;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(total);
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            let take = pos + 1;
            if total.saturating_add(take) > max_bytes {
                bail!("guest control line exceeds {max_bytes} bytes");
            }
            buf.extend_from_slice(&available[..take]);
            reader.consume(take);
            return Ok(total + take);
        }
        let len = available.len();
        if total.saturating_add(len) > max_bytes {
            bail!("guest control line exceeds {max_bytes} bytes");
        }
        buf.extend_from_slice(available);
        reader.consume(len);
        total += len;
    }
}

fn trim_line_end(mut bytes: &[u8]) -> &[u8] {
    while let Some(&b) = bytes.last() {
        if b == b'\n' || b == b'\r' {
            bytes = &bytes[..bytes.len() - 1];
        } else {
            break;
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lossless_json_rejects_excessive_nesting() {
        let mut v = serde_json::json!(1);
        for _ in 0..70 {
            v = serde_json::json!([v]);
        }
        assert!(validate_lossless_json(&v, MAX_JSON_DEPTH).is_err());
    }

    #[test]
    fn output_ledger_truncates_on_utf8_boundary_and_merges_text() {
        let mut ledger = OutputLedger::new(7);
        assert!(!ledger.push_text("ab"));
        assert!(!ledger.push_text("cd"));
        assert!(ledger.push_text("你好"));
        assert!(ledger.truncated);
        assert_eq!(
            ledger.items,
            vec![PtcOutputItem::Text("abcd你".to_string())]
        );
    }
}
