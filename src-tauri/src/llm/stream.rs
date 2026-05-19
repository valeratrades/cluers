//! SSE chunk parser and JSON-path delta extraction.
//!
//! The transport contract: the upstream LLM API streams `text/event-stream`
//! frames of the form `data: <json>\n` with `data: [DONE]` terminating the
//! stream. Some providers may omit the trailing newline on the final frame.
//! Lines that don't start with `data:` are ignored; malformed JSON inside
//! a frame is also silently dropped (matches the JS behavior).

use crate::llm::{LlmError, StreamEvent};
use futures_util::StreamExt;
use tauri::ipc::Channel;
use tokio::sync::oneshot;

pub struct StreamOutcome {
    pub full_response: String,
    pub usage: Option<serde_json::Value>,
}

pub async fn stream_sse(
    response: reqwest::Response,
    channel: &Channel<StreamEvent>,
    cancel_rx: &mut oneshot::Receiver<()>,
    extract_delta: impl Fn(&serde_json::Value) -> Option<String>,
) -> Result<StreamOutcome, LlmError> {
    let mut full_response = String::new();
    let mut usage: Option<serde_json::Value> = None;
    let mut buffer = String::new();
    let mut stream = response.bytes_stream();

    loop {
        let next = tokio::select! {
            biased;
            _ = &mut *cancel_rx => return Err(LlmError::Cancelled),
            n = stream.next() => n,
        };
        match next {
            None => break,
            Some(Err(e)) => return Err(LlmError::Reqwest(e)),
            Some(Ok(bytes)) => {
                buffer.push_str(&String::from_utf8_lossy(&bytes));
                while let Some(idx) = buffer.find('\n') {
                    let line: String = buffer[..idx].to_string();
                    buffer.drain(..=idx);
                    process_line(
                        &line,
                        channel,
                        &extract_delta,
                        &mut full_response,
                        &mut usage,
                    )?;
                }
            }
        }
    }
    // Flush any unterminated final frame.
    if !buffer.is_empty() {
        let line = std::mem::take(&mut buffer);
        process_line(
            &line,
            channel,
            &extract_delta,
            &mut full_response,
            &mut usage,
        )?;
    }

    Ok(StreamOutcome {
        full_response,
        usage,
    })
}

fn process_line(
    line: &str,
    channel: &Channel<StreamEvent>,
    extract_delta: &impl Fn(&serde_json::Value) -> Option<String>,
    full_response: &mut String,
    usage: &mut Option<serde_json::Value>,
) -> Result<(), LlmError> {
    let trimmed = line.trim();
    let Some(rest) = trimmed.strip_prefix("data:") else {
        return Ok(());
    };
    let payload = rest.trim();
    if payload.is_empty() || payload == "[DONE]" {
        return Ok(());
    }
    // Malformed JSON in a single frame: silently drop. The provider may
    // legitimately split a JSON object across multiple network chunks,
    // in which case the next iteration will reassemble it via the
    // newline-buffered loop.
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(payload) else {
        return Ok(());
    };
    if usage.is_none() {
        if let Some(u) = parsed.get("usage").filter(|u| !u.is_null()) {
            *usage = Some(u.clone());
        }
    }
    if let Some(delta) = extract_delta(&parsed) {
        if !delta.is_empty() {
            full_response.push_str(&delta);
            channel
                .send(StreamEvent::Chunk { delta })
                .map_err(|e| LlmError::Channel(e.to_string()))?;
        }
    }
    Ok(())
}

/// Resolve a dotted/bracketed JSON path like `choices[0].delta.content`.
/// Returns the string value at that path, or `None` if missing or
/// non-string.
pub fn extract_by_path(value: &serde_json::Value, path: &str) -> Option<String> {
    if path.is_empty() {
        return value.as_str().map(String::from);
    }
    let mut current = value;
    let normalized: String = path
        .chars()
        .map(|c| match c {
            '[' => '.',
            ']' => '\0',
            other => other,
        })
        .filter(|c| *c != '\0')
        .collect();
    for key in normalized.split('.') {
        if key.is_empty() {
            continue;
        }
        current = if let Ok(idx) = key.parse::<usize>() {
            current.as_array()?.get(idx)?
        } else {
            current.get(key)?
        };
    }
    current.as_str().map(String::from)
}

/// Streaming-delta extractor that mirrors the JS `getStreamingContent`:
/// try `default_path` with `.message.` → `.delta.` swapped, then a set
/// of known-good fallbacks, then `default_path` itself.
pub fn extract_streaming_delta(
    parsed: &serde_json::Value,
    default_path: &str,
) -> Option<String> {
    let modified = default_path.replace(".message.", ".delta.");
    let paths: [&str; 6] = [
        modified.as_str(),
        "choices[0].delta.content",
        "candidates[0].content.parts[0].text",
        "delta.text",
        "text",
        default_path,
    ];
    for p in paths {
        if let Some(s) = extract_by_path(parsed, p) {
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}
