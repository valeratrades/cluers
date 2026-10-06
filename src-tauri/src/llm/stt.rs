//! Speech-to-text: one entry point for the Pluely-hosted and custom-template paths.

use base64::Engine as _;
use reqwest::multipart::{Form, Part};
use tauri::AppHandle;

use crate::llm::{
    commands::ProviderInput,
    provider::{method, parse_curl, resolve_vars, substitute_string, substitute_value},
    secrets::{ProviderKind, Secrets},
    stream::extract_by_path,
    LlmError, LlmState,
};

/// Entry point for every STT caller.
/// `Ok("")` means the provider heard no speech.
pub async fn transcribe(
    app: &AppHandle,
    llm: &LlmState,
    provider: &ProviderInput,
    audio: &[u8],
    mime: &str,
) -> Result<String, LlmError> {
    if provider.is_pluely_hosted {
        crate::api::transcribe_pluely(app, &llm.http, audio)
            .await
            .map_err(LlmError::PluelyStt)
    } else {
        transcribe_custom(&llm.http, &llm.secrets, provider, audio, mime).await
    }
}

async fn transcribe_custom(
    http: &reqwest::Client,
    secrets: &Secrets,
    p: &ProviderInput,
    audio: &[u8],
    mime: &str,
) -> Result<String, LlmError> {
    if !p.curl.contains("{{AUDIO}}") {
        return Err(LlmError::UnsupportedAttachment("AUDIO"));
    }
    let parsed = parse_curl(&p.curl)?;
    let mut vars = resolve_vars(secrets, ProviderKind::Stt, p).await?;
    let url = substitute_string(&parsed.url, &vars);
    let multipart = !parsed.form.is_empty();

    let mut req = http.request(method(&parsed.method)?, url);
    for (k, v) in &parsed.headers {
        if multipart && k.eq_ignore_ascii_case("content-type") {
            continue; // reqwest must set the multipart boundary
        }
        req = req.header(k, substitute_string(v, &vars));
    }

    req = if multipart {
        let mut form = Form::new();
        for (k, v) in &parsed.form {
            form = if v.contains("{{AUDIO}}") {
                let part = Part::bytes(audio.to_vec())
                    .file_name("audio.wav")
                    .mime_str(mime)?;
                form.part(k.clone(), part)
            } else {
                form.text(k.clone(), substitute_string(v, &vars))
            };
        }
        req.multipart(form)
    } else {
        match parsed.data.as_deref() {
            Some(d) if d.trim() == "{{AUDIO}}" => req.body(audio.to_vec()),
            Some(d) => {
                let mut v: serde_json::Value = serde_json::from_str(d)
                    .map_err(|e| LlmError::CurlParse(format!("body json: {e}")))?;
                vars.insert(
                    "AUDIO".to_string(),
                    base64::engine::general_purpose::STANDARD.encode(audio),
                );
                substitute_value(&mut v, &vars);
                req.json(&v)
            }
            None => {
                return Err(LlmError::InvalidCurl(
                    "STT template needs -F, -d or --data-binary",
                ))
            }
        }
    };

    let resp = req.send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(LlmError::ProviderApi {
            status: status.as_u16(),
            body: text,
        });
    }
    tracing::debug!(provider = %p.id, body = %text, "stt response");

    let json: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| LlmError::SttResponse(format!("not JSON: {text}")))?;
    extract_by_path(&json, &p.response_content_path).ok_or_else(|| {
        LlmError::SttResponse(format!(
            "`{}` is not a string in {text}",
            p.response_content_path
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::secrets::tests::install;
    use std::collections::HashMap;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    const AUDIO: &[u8] = b"RIFF\x00\x01fakewav";

    /// Serves exactly one request; returns it raw (head + body).
    async fn accept_one(listener: &TcpListener, status: u16, body: &str) -> Vec<u8> {
        let (sock, _) = listener.accept().await.unwrap();
        let mut rd = BufReader::new(sock);
        let mut head = Vec::new();
        let mut len = None;
        loop {
            let mut line = String::new();
            rd.read_line(&mut line).await.unwrap();
            let lower = line.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("content-length:") {
                len = Some(v.trim().parse::<usize>().unwrap());
            }
            head.extend_from_slice(line.as_bytes());
            if line == "\r\n" {
                break;
            }
        }
        let mut req_body = vec![0; len.expect("request has Content-Length")];
        rd.read_exact(&mut req_body).await.unwrap();
        head.extend_from_slice(&req_body);
        let resp = format!(
            "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        rd.get_mut().write_all(resp.as_bytes()).await.unwrap();
        head
    }

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    struct Case {
        id: &'static str,
        curl: &'static str,
        vars: &'static [(&'static str, &'static str)],
        response: &'static str,
        path: &'static str,
        expect_req: &'static [&'static [u8]],
        expect: Result<&'static str, &'static str>, // Err = LlmError variant name
    }

    async fn run(case: Case) {
        let _ = install();
        let secrets = Secrets::new();
        secrets
            .set(ProviderKind::Stt, case.id, "API_KEY", "k")
            .await
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let p = ProviderInput {
            id: case.id.to_string(),
            curl: case.curl.replace("HOST", &addr.to_string()),
            response_content_path: case.path.to_string(),
            streaming: false,
            is_pluely_hosted: false,
            user_variables: case
                .vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<HashMap<_, _>>(),
        };
        let http = reqwest::Client::new();
        let (req, res) = tokio::join!(
            accept_one(&listener, 200, case.response),
            transcribe_custom(&http, &secrets, &p, AUDIO, "audio/wav")
        );
        for needle in case.expect_req {
            assert!(
                contains(&req, needle),
                "{}: request lacks {:?}:\n{}",
                case.id,
                String::from_utf8_lossy(needle),
                String::from_utf8_lossy(&req)
            );
        }
        match (case.expect, res) {
            (Ok(want), Ok(got)) => assert_eq!(got, want, "{}", case.id),
            (Err(want), Err(e)) => assert!(format!("{e:?}").starts_with(want), "{}: {e:?}", case.id),
            (want, got) => panic!("{}: want {want:?}, got {got:?}", case.id),
        }
    }

    #[tokio::test]
    async fn templates() {
        let cases = [
            Case {
                id: "stt_openai",
                curl: r#"curl -X POST "http://HOST/v1/audio/transcriptions" -H "Authorization: Bearer {{API_KEY}}" -H "Content-Type: multipart/form-data" -F "file={{AUDIO}}" -F "model={{MODEL}}""#,
                vars: &[("model", "whisper-1")],
                response: r#"{"text":"hi"}"#,
                path: "text",
                expect_req: &[
                    b"authorization: Bearer k",
                    b"name=\"file\"; filename=\"audio.wav\"",
                    AUDIO,
                    b"name=\"model\"\r\n\r\nwhisper-1",
                ],
                expect: Ok("hi"),
            },
            Case {
                id: "stt_speechmatics",
                curl: r#"curl "http://HOST/v2/jobs" -H "Authorization: Bearer {{API_KEY}}" -F "data_file={{AUDIO}}""#,
                vars: &[],
                response: r#"{"job":{"id":"j"}}"#,
                path: "job.id",
                expect_req: &[b"name=\"data_file\"; filename=\"audio.wav\""],
                expect: Ok("j"),
            },
            Case {
                id: "stt_deepgram",
                curl: r#"curl -X POST "http://HOST/v1/listen?model={{MODEL}}" -H "Authorization: TOKEN {{API_KEY}}" -H "Content-Type: audio/wav" --data-binary {{AUDIO}}"#,
                vars: &[("MODEL", "nova")],
                response: r#"{"results":{"channels":[{"alternatives":[{"transcript":"yo"}]}]}}"#,
                path: "results.channels[0].alternatives[0].transcript",
                expect_req: &[b"POST /v1/listen?model=nova ", b"\r\n\r\nRIFF\x00\x01fakewav"],
                expect: Ok("yo"),
            },
            Case {
                id: "stt_google",
                curl: r#"curl -X POST "http://HOST/v1/speech:recognize" -H "Authorization: Bearer {{API_KEY}}" -d '{"config":{"languageCode":"en-US"},"audio":{"content":"{{AUDIO}}"}}'"#,
                vars: &[],
                response: r#"{"results":[{"alternatives":[{"transcript":"g"}]}]}"#,
                path: "results[0].alternatives[0].transcript",
                expect_req: &[b"\"content\":\"UklGRgABZmFrZXdhdg==\""],
                expect: Ok("g"),
            },
            Case {
                id: "stt_text_body",
                curl: r#"curl "http://HOST/t" -H "Authorization: Bearer {{API_KEY}}" -F "file={{AUDIO}}""#,
                vars: &[],
                response: "hello",
                path: "text",
                expect_req: &[],
                expect: Err("SttResponse"),
            },
            Case {
                id: "stt_silence",
                curl: r#"curl "http://HOST/t" -H "Authorization: Bearer {{API_KEY}}" -F "file={{AUDIO}}""#,
                vars: &[],
                response: r#"{"text":""}"#,
                path: "text",
                expect_req: &[],
                expect: Ok(""),
            },
            Case {
                id: "stt_bad_path",
                curl: r#"curl "http://HOST/t" -H "Authorization: Bearer {{API_KEY}}" -F "file={{AUDIO}}""#,
                vars: &[],
                response: r#"{"foo":1}"#,
                path: "text",
                expect_req: &[],
                expect: Err("SttResponse"),
            },
        ];
        for c in cases {
            run(c).await;
        }
    }

    #[tokio::test]
    async fn ai_secret_does_not_leak_into_stt() {
        let _ = install();
        let id = "stt_kind_leak";
        let secrets = Secrets::new();
        secrets.set(ProviderKind::Ai, id, "API_KEY", "ai").await.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p = ProviderInput {
            id: id.to_string(),
            curl: format!(
                r#"curl "http://{}/t" -H "Authorization: Bearer {{{{API_KEY}}}}" -F "file={{{{AUDIO}}}}""#,
                listener.local_addr().unwrap()
            ),
            response_content_path: "text".to_string(),
            streaming: false,
            is_pluely_hosted: false,
            user_variables: HashMap::new(),
        };
        let http = reqwest::Client::new();
        tokio::select! {
            r = transcribe_custom(&http, &secrets, &p, AUDIO, "audio/wav") => {
                assert!(matches!(r, Err(LlmError::MissingVariable(ref v)) if v == "API_KEY"), "{r:?}");
            }
            _ = listener.accept() => panic!("request sent without an STT key"),
        }
    }
}
