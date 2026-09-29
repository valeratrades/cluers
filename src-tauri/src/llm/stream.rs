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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tauri::ipc::InvokeResponseBody;

    async fn run(chunks: Vec<Vec<u8>>) -> Result<(StreamOutcome, Vec<String>), LlmError> {
        let body = reqwest::Body::wrap_stream(futures_util::stream::iter(
            chunks.into_iter().map(Ok::<_, std::io::Error>),
        ));
        let response = reqwest::Response::from(tauri::http::Response::new(body));
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let channel = Channel::<StreamEvent>::new(move |body| {
            match body {
                InvokeResponseBody::Json(s) => sink.lock().unwrap().push(s),
                InvokeResponseBody::Raw(_) => panic!("StreamEvent serializes to json"),
            }
            Ok(())
        });
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        let outcome = stream_sse(response, &channel, &mut cancel_rx, |v| {
            extract_by_path(v, "choices[0].delta.content")
        })
        .await;
        drop(cancel_tx);
        let events = events.lock().unwrap().clone();
        outcome.map(|o| (o, events))
    }

    #[tokio::test]
    async fn utf8_split_at_every_offset() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"café 日本 🎉\"}}]}\n\ndata: [DONE]\n\n"
            .as_bytes()
            .to_vec();
        let (whole, whole_events) = run(vec![body.clone()]).await.unwrap();
        assert_eq!(whole.full_response, "café 日本 🎉");
        assert_eq!(whole_events.len(), 1);

        let mut splits: Vec<Vec<Vec<u8>>> = (1..body.len())
            .map(|i| vec![body[..i].to_vec(), body[i..].to_vec()])
            .collect();
        splits.push(body.iter().map(|b| vec![*b]).collect());
        for chunks in splits {
            let (o, events) = run(chunks).await.unwrap();
            assert_eq!(o.full_response, whole.full_response);
            assert_eq!(events, whole_events);
        }
    }

    #[tokio::test]
    async fn frames() {
        let d = |s: &str| format!("{{\"choices\":[{{\"delta\":{{\"content\":\"{s}\"}}}}]}}");
        let cases: Vec<(Vec<u8>, &str)> = vec![
            (format!("data: {}\r\n\r\ndata: {}\r\n\r\n", d("a"), d("b")).into_bytes(), "ab"),
            (b"data: {\"choices\":[{\"delta\":\ndata: {\"content\":\"x\"}}]}\n\n".to_vec(), "x"),
            (
                format!(": keepalive\nevent: foo\nid: 1\nretry: 10\ndata: {}\n\n", d("y")).into_bytes(),
                "y",
            ),
            (b"data: [DONE]\n\n".to_vec(), ""),
            (format!("data: {}\n\ndata: {}", d("p"), d("q")).into_bytes(), "pq"),
        ];
        for (input, expected) in cases {
            let (o, _) = run(vec![input.clone()]).await.unwrap();
            assert_eq!(o.full_response, expected, "{}", String::from_utf8_lossy(&input));
        }
    }

    #[tokio::test]
    async fn usage_captured() {
        let (o, _) = run(vec![b"data: {\"choices\":[],\"usage\":{\"total_tokens\":3}}\n\n".to_vec()])
            .await
            .unwrap();
        assert_eq!(o.usage, Some(serde_json::json!({"total_tokens": 3})));
    }

    #[tokio::test]
    async fn invalid_frames_error() {
        let cases: Vec<Vec<u8>> = vec![
            b"data: {\"choices\":[{\"delta\":{\"content\":\"\xff\"}}]}\n\n".to_vec(),
            b"data: {nope\n\n".to_vec(),
        ];
        for input in cases {
            assert!(matches!(run(vec![input]).await, Err(LlmError::Json(_))));
        }
    }
}
