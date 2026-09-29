//! SSE chunk parser and JSON-path delta extraction.
//!
//! An event is one or more `data:` lines (LF or CRLF) ended by a blank line;
//! other fields are ignored and `data: [DONE]` is a no-op. Some providers omit
//! the final newline, so a pending event is flushed at EOF. Bytes are decoded
//! only as complete JSON payloads; invalid UTF-8 or malformed JSON is an error.

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
    let mut buf: Vec<u8> = Vec::new();
    let mut data: Option<Vec<u8>> = None;
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
                buf.extend_from_slice(&bytes);
                let mut start = 0;
                while let Some(i) = buf[start..].iter().position(|&b| b == b'\n') {
                    process_line(
                        &buf[start..start + i],
                        &mut data,
                        channel,
                        &extract_delta,
                        &mut full_response,
                        &mut usage,
                    )?;
                    start += i + 1;
                }
                buf.drain(..start);
            }
        }
    }
    if !buf.is_empty() {
        process_line(
            &buf,
            &mut data,
            channel,
            &extract_delta,
            &mut full_response,
            &mut usage,
        )?;
    }
    if let Some(d) = data {
        dispatch(&d, channel, &extract_delta, &mut full_response, &mut usage)?;
    }

    Ok(StreamOutcome {
        full_response,
        usage,
    })
}

fn process_line(
    line: &[u8],
    data: &mut Option<Vec<u8>>,
    channel: &Channel<StreamEvent>,
    extract_delta: &impl Fn(&serde_json::Value) -> Option<String>,
    full_response: &mut String,
    usage: &mut Option<serde_json::Value>,
) -> Result<(), LlmError> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.is_empty() {
        if let Some(d) = data.take() {
            dispatch(&d, channel, extract_delta, full_response, usage)?;
        }
    } else if let Some(value) = line.strip_prefix(b"data:") {
        let value = value.strip_prefix(b" ").unwrap_or(value);
        match data {
            Some(d) => {
                d.push(b'\n');
                d.extend_from_slice(value);
            }
            None => *data = Some(value.to_vec()),
        }
    }
    Ok(())
}

fn dispatch(
    payload: &[u8],
    channel: &Channel<StreamEvent>,
    extract_delta: &impl Fn(&serde_json::Value) -> Option<String>,
    full_response: &mut String,
    usage: &mut Option<serde_json::Value>,
) -> Result<(), LlmError> {
    let payload = payload.trim_ascii();
    if payload.is_empty() || payload == b"[DONE]" {
        return Ok(());
    }
    let parsed: serde_json::Value = serde_json::from_slice(payload)?;
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
