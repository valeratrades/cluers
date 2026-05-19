//! Curl-template parser, variable substitution, message building, and
//! the custom-provider stream entrypoint.
//!
//! The supported curl subset (intentionally narrow — matches what
//! `@bany/curl-to-json` accepted on the JS side):
//! - `-X METHOD` / `--request METHOD`
//! - `-H 'K: V'` / `--header 'K: V'` (quoted, double-quoted, or bare)
//! - `-d <data>` / `--data <data>` / `--data-raw <data>` / `--data-binary <data>`
//! - `--url <url>` and the bare positional URL
//!
//! Variable templates use `{{UPPER_SNAKE}}` placeholders. Reserved names
//! (`TEXT`, `IMAGE`, `IMAGE_MIME`, `AUDIO`, `DOCUMENT`, `SYSTEM_PROMPT`)
//! are handled by `build_messages` / `substitute_value`; everything else
//! comes from keychain-stored secrets ∪ user_variables.

use crate::db::schema::AttachedFile;
use crate::llm::{commands::StreamChatRequest, secrets, stream, LlmError, StreamEvent};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tauri::ipc::Channel;
use tokio::sync::oneshot;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryMessage {
    pub role: crate::db::schema::Role,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct ParsedCurl {
    pub url: String,
    pub method: String,
    pub headers: HashMap<String, String>,
    pub body: Option<serde_json::Value>,
}

pub fn parse_curl(input: &str) -> Result<ParsedCurl, LlmError> {
    let tokens = tokenize(input)?;
    let mut iter = tokens.into_iter().peekable();
    if iter.peek().map(|s| s.as_str()) == Some("curl") {
        iter.next();
    }
    let mut url: Option<String> = None;
    let mut method: Option<String> = None;
    let mut headers: HashMap<String, String> = HashMap::new();
    let mut body: Option<String> = None;

    while let Some(tok) = iter.next() {
        match tok.as_str() {
            "-X" | "--request" => {
                method = Some(iter.next().ok_or(LlmError::InvalidCurl("missing -X value"))?);
            }
            "-H" | "--header" => {
                let v = iter.next().ok_or(LlmError::InvalidCurl("missing -H value"))?;
                let (k, val) = v.split_once(':').ok_or(LlmError::InvalidCurl("bad header"))?;
                headers.insert(k.trim().to_string(), val.trim().to_string());
            }
            "-d" | "--data" | "--data-raw" | "--data-binary" => {
                body = Some(iter.next().ok_or(LlmError::InvalidCurl("missing -d value"))?);
            }
            "--url" => {
                url = Some(iter.next().ok_or(LlmError::InvalidCurl("missing --url value"))?);
            }
            other if !other.starts_with('-') => {
                if url.is_none() {
                    url = Some(other.to_string());
                }
            }
            // Flags we don't model (e.g. `-i`, `--compressed`) are skipped.
            _ => {}
        }
    }

    let url = url.ok_or(LlmError::InvalidCurl("missing URL"))?;
    let method = method.unwrap_or_else(|| {
        if body.is_some() {
            "POST".to_string()
        } else {
            "GET".to_string()
        }
    });
    let body_json = match body {
        None => None,
        Some(s) => Some(
            serde_json::from_str(&s)
                .map_err(|e| LlmError::CurlParse(format!("body json: {e}")))?,
        ),
    };
    Ok(ParsedCurl {
        url,
        method,
        headers,
        body: body_json,
    })
}

fn tokenize(input: &str) -> Result<Vec<String>, LlmError> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut chars = input.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;

    while let Some(c) = chars.next() {
        match c {
            '\\' if in_double => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            '\\' if !in_single => {
                // Outside quotes, `\\\n` is line continuation; otherwise
                // take the next char literally.
                if chars.peek() == Some(&'\n') {
                    chars.next();
                } else if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            c if c.is_whitespace() && !in_single && !in_double => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if in_single || in_double {
        return Err(LlmError::InvalidCurl("unterminated quote"));
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

pub fn extract_variables(template: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        rest = &rest[start + 2..];
        let Some(end) = rest.find("}}") else { break };
        let name = &rest[..end];
        if !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_uppercase() || c == '_')
            && !out.iter().any(|v| v == name)
        {
            out.push(name.to_string());
        }
        rest = &rest[end + 2..];
    }
    out
}

pub fn substitute_string(template: &str, vars: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str("{{");
            out.push_str(after);
            return out;
        };
        let name = &after[..end];
        if let Some(v) = vars.get(name) {
            out.push_str(v);
        } else {
            out.push_str("{{");
            out.push_str(name);
            out.push_str("}}");
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

pub fn substitute_value(value: &mut serde_json::Value, vars: &HashMap<String, String>) {
    match value {
        serde_json::Value::String(s) => *s = substitute_string(s, vars),
        serde_json::Value::Array(a) => {
            for v in a {
                substitute_value(v, vars);
            }
        }
        serde_json::Value::Object(o) => {
            for v in o.values_mut() {
                substitute_value(v, vars);
            }
        }
        _ => {}
    }
}

/// Mutates `body[messages_key]` in place, replacing the `{{TEXT}}`
/// template message with the actual user turn (text + attachments) and
/// inserting `history` before it. `messages_key` is auto-detected from
/// `{messages, contents, conversation, history}`.
pub fn build_messages(
    body: &mut serde_json::Value,
    history: &[HistoryMessage],
    user_message: &str,
    attached_files: &[AttachedFile],
) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let key: Option<String> = ["messages", "contents", "conversation", "history"]
        .iter()
        .find(|k| obj.get(**k).map(|v| v.is_array()).unwrap_or(false))
        .map(|s| s.to_string());
    let Some(key) = key else { return };
    let Some(template_arr) = obj.get(&key).and_then(|v| v.as_array()).cloned() else {
        return;
    };

    let user_idx = template_arr
        .iter()
        .position(|m| value_contains(m, "{{TEXT}}"));

    let (prefix, user_tpl, suffix) = match user_idx {
        Some(i) => {
            let mut head = template_arr;
            let suffix = head.split_off(i + 1);
            let user_tpl = head.pop().unwrap();
            (head, Some(user_tpl), suffix)
        }
        None => (template_arr, None, Vec::new()),
    };

    let history_msgs: Vec<serde_json::Value> = history
        .iter()
        .map(|m| {
            serde_json::json!({
                "role": m.role.as_str(),
                "content": m.content,
            })
        })
        .collect();

    let mut result: Vec<serde_json::Value> = Vec::with_capacity(prefix.len() + history_msgs.len() + 1 + suffix.len());
    result.extend(prefix);
    result.extend(history_msgs);
    match user_tpl {
        Some(tpl) => result.push(expand_user_template(tpl, user_message, attached_files)),
        None => result.push(serde_json::json!({ "role": "user", "content": user_message })),
    }
    result.extend(suffix);

    obj.insert(key, serde_json::Value::Array(result));
}

fn value_contains(v: &serde_json::Value, needle: &str) -> bool {
    match v {
        serde_json::Value::String(s) => s.contains(needle),
        serde_json::Value::Array(a) => a.iter().any(|x| value_contains(x, needle)),
        serde_json::Value::Object(o) => o.values().any(|x| value_contains(x, needle)),
        _ => false,
    }
}

fn json_escape_contents(s: &str) -> String {
    // Strip the outer quotes from the JSON-stringified form so the
    // result is safe to substitute *into* a JSON-string literal.
    let escaped = serde_json::to_string(s).unwrap_or_default();
    if escaped.len() < 2 {
        String::new()
    } else {
        escaped[1..escaped.len() - 1].to_string()
    }
}

fn expand_user_template(
    template: serde_json::Value,
    user_message: &str,
    attached: &[AttachedFile],
) -> serde_json::Value {
    // {{TEXT}} substitution via JSON string-level replace so the result
    // remains valid JSON (escapes user newlines, quotes, etc).
    let tpl_str = serde_json::to_string(&template).unwrap_or_default();
    let with_text = tpl_str.replace("{{TEXT}}", &json_escape_contents(user_message));
    let value: serde_json::Value = serde_json::from_str(&with_text).unwrap_or(template);

    let images: Vec<HashMap<String, String>> = attached
        .iter()
        .filter(|f| f.mime.starts_with("image/"))
        .map(|f| {
            let mut m = HashMap::new();
            m.insert("IMAGE".to_string(), f.base64.clone());
            m.insert("IMAGE_MIME".to_string(), f.mime.clone());
            m
        })
        .collect();
    let docs: Vec<HashMap<String, String>> = attached
        .iter()
        .filter(|f| !f.mime.starts_with("image/"))
        .map(|f| {
            let mut m = HashMap::new();
            m.insert("DOCUMENT".to_string(), f.base64.clone());
            m
        })
        .collect();

    replacer(value, &images, &docs)
}

fn replacer(
    node: serde_json::Value,
    images: &[HashMap<String, String>],
    docs: &[HashMap<String, String>],
) -> serde_json::Value {
    match node {
        serde_json::Value::Array(arr) => {
            let arr = expand_one_array(arr, &["IMAGE", "IMAGE_MIME"], images);
            let arr = expand_one_array(arr, &["DOCUMENT"], docs);
            serde_json::Value::Array(
                arr.into_iter()
                    .map(|v| replacer(v, images, docs))
                    .collect(),
            )
        }
        serde_json::Value::Object(obj) => serde_json::Value::Object(
            obj.into_iter()
                .map(|(k, v)| (k, replacer(v, images, docs)))
                .collect(),
        ),
        other => other,
    }
}

fn expand_one_array(
    arr: Vec<serde_json::Value>,
    tokens: &[&str],
    payloads: &[HashMap<String, String>],
) -> Vec<serde_json::Value> {
    let idx = arr.iter().position(|item| {
        let s = serde_json::to_string(item).unwrap_or_default();
        tokens
            .iter()
            .any(|t| s.contains(&format!("{{{{{}}}}}", t)))
    });
    let Some(idx) = idx else { return arr };

    let tpl_str = serde_json::to_string(&arr[idx]).unwrap_or_default();
    let parts: Vec<serde_json::Value> = payloads
        .iter()
        .map(|p| {
            let mut s = tpl_str.clone();
            for token in tokens {
                let val = p.get(*token).map(String::as_str).unwrap_or("");
                s = s.replace(
                    &format!("{{{{{}}}}}", token),
                    &json_escape_contents(val),
                );
            }
            serde_json::from_str(&s).unwrap_or(serde_json::Value::Null)
        })
        .collect();
    let mut out = Vec::with_capacity(arr.len() - 1 + parts.len());
    for (i, item) in arr.into_iter().enumerate() {
        if i == idx {
            out.extend(parts.clone());
        } else {
            out.push(item);
        }
    }
    out
}

/// Custom-provider stream entrypoint (the non-Pluely path).
pub async fn stream_custom(
    http: &reqwest::Client,
    request: StreamChatRequest,
    channel: &Channel<StreamEvent>,
    cancel_rx: &mut oneshot::Receiver<()>,
) -> Result<String, LlmError> {
    let p = &request.provider;
    let parsed = parse_curl(&p.curl)?;

    // Merge keychain secrets + non-secret user_variables. Keychain wins.
    let mut vars: HashMap<String, String> = p
        .user_variables
        .iter()
        .map(|(k, v)| (k.to_ascii_uppercase(), v.clone()))
        .collect();
    for name in secrets::list_provider_secret_names(&p.id)? {
        if let Some(v) = secrets::get_provider_secret(&p.id, &name)? {
            vars.insert(name, v);
        }
    }
    vars.insert(
        "SYSTEM_PROMPT".to_string(),
        request.system_prompt.clone().unwrap_or_default(),
    );

    // Required-variable check (excludes reserved per-message tokens).
    for v in extract_variables(&p.curl) {
        if matches!(
            v.as_str(),
            "SYSTEM_PROMPT" | "TEXT" | "IMAGE" | "IMAGE_MIME" | "AUDIO" | "DOCUMENT"
        ) {
            continue;
        }
        if vars
            .get(&v)
            .map(|s| s.trim().is_empty())
            .unwrap_or(true)
        {
            return Err(LlmError::MissingVariable(v));
        }
    }

    // Build body (messages + attachments + variable subs).
    let mut body = parsed.body.unwrap_or_else(|| serde_json::json!({}));
    build_messages(
        &mut body,
        &request.history,
        &request.message,
        &request.attached_files,
    );
    substitute_value(&mut body, &vars);

    if p.streaming {
        if let Some(obj) = body.as_object_mut() {
            let existing = obj.keys().find(|k| k.eq_ignore_ascii_case("stream")).cloned();
            match existing {
                Some(k) => {
                    obj.insert(k, serde_json::json!(true));
                }
                None => {
                    obj.insert("stream".to_string(), serde_json::json!(true));
                }
            }
        }
    }

    let url = substitute_string(&parsed.url, &vars);
    let method = match parsed.method.to_ascii_uppercase().as_str() {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "PUT" => reqwest::Method::PUT,
        "PATCH" => reqwest::Method::PATCH,
        "DELETE" => reqwest::Method::DELETE,
        other => {
            return Err(LlmError::CurlParse(format!("unsupported method: {other}")))
        }
    };

    let mut req_builder = http.request(method.clone(), url);
    for (k, v) in &parsed.headers {
        req_builder = req_builder.header(k, substitute_string(v, &vars));
    }
    req_builder = req_builder.header("Content-Type", "application/json");

    let send_fut = if method == reqwest::Method::GET {
        req_builder.send()
    } else {
        req_builder.json(&body).send()
    };

    let response = tokio::select! {
        biased;
        _ = &mut *cancel_rx => return Err(LlmError::Cancelled),
        r = send_fut => r?,
    };

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        return Err(LlmError::ProviderApi { status, body });
    }

    if !p.streaming {
        let json: serde_json::Value = response.json().await?;
        let content =
            stream::extract_by_path(&json, &p.response_content_path).unwrap_or_default();
        if !content.is_empty() {
            channel
                .send(StreamEvent::Chunk {
                    delta: content.clone(),
                })
                .map_err(|e| LlmError::Channel(e.to_string()))?;
        }
        return Ok(content);
    }

    let path = p.response_content_path.clone();
    let outcome = stream::stream_sse(response, channel, cancel_rx, move |parsed| {
        stream::extract_streaming_delta(parsed, &path)
    })
    .await?;
    Ok(outcome.full_response)
}
