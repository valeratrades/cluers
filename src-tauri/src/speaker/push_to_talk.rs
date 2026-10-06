//! Push-to-talk: `record_push_to_talk` owns one mic recording until `finish_push_to_talk` ends it.
use super::commands::{normalize_audio_level, samples_to_wav, STT_TIMEOUT};
use super::vad::calculate_audio_metrics;
use super::SpeakerInput;
use crate::llm::commands::ProviderInput;
use crate::llm::{stt, LlmState};
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager};
use tokio::sync::oneshot;

const MAX_SECS: usize = 180;

#[derive(Default)]
pub struct PushToTalk(std::sync::Mutex<Option<oneshot::Sender<Finish>>>);

#[derive(Debug, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum Finish {
    Send,
    Discard,
}

#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PttEvent {
    Started,
    Level { rms: f32 },
}

impl PushToTalk {
    fn register(&self) -> Result<oneshot::Receiver<Finish>, String> {
        let mut slot = self.0.lock().expect("no panics while held");
        if slot.as_ref().is_some_and(|s| !s.is_closed()) {
            return Err("Already recording".into());
        } // a closed sender is a recording that ended on its own
        let (tx, rx) = oneshot::channel();
        *slot = Some(tx);
        Ok(rx)
    }
}

/// `Ok(None)`: discarded. `Ok(Some(""))`: the provider heard no speech.
#[tauri::command]
pub async fn record_push_to_talk(
    app: AppHandle,
    device_id: Option<String>,
    stt: ProviderInput,
    events: Channel<PttEvent>,
) -> Result<Option<String>, String> {
    let finish = app.state::<PushToTalk>().register()?;
    let send = |e| {
        events
            .send(e)
            .map_err(|e| format!("Push-to-talk channel closed: {e}"))
    };
    send(PttEvent::Started)?; // finish is accepted from here on
    let mut mic = SpeakerInput::microphone(device_id)
        .map_err(|e| format!("Failed to access microphone: {e:#}"))?
        .stream();
    let sr = mic.sample_rate();
    let samples = record(&mut mic, finish, sr as usize * MAX_SECS, |rms| {
        send(PttEvent::Level { rms })
    })
    .await
    .map_err(|e| match mic.error() {
        Some(cause) => format!("{e}: {cause:#}"),
        None => e,
    })?;
    drop(mic);
    let Some(samples) = samples else {
        return Ok(None);
    };
    let wav = samples_to_wav(sr, &normalize_audio_level(&samples, 0.1));
    let llm = app.state::<LlmState>();
    tokio::time::timeout(
        STT_TIMEOUT,
        stt::transcribe(&app, &llm, &stt, &wav, "audio/wav"),
    )
    .await
    .map_err(|_| "Speech transcription timed out (30s)".to_string())?
    .map(Some)
    .map_err(|e| format!("Transcription failed: {e}"))
}

#[tauri::command]
pub fn finish_push_to_talk(app: AppHandle, action: Finish) -> Result<(), String> {
    let tx = app
        .state::<PushToTalk>()
        .0
        .lock()
        .expect("no panics while held")
        .take();
    tx.ok_or("Not recording")?
        .send(action)
        .map_err(|_| "Recording already ended".to_string())
}

/// Collects `mic` until `finish`, or until `limit` samples, which sends.
async fn record<S: Stream<Item = f32> + Unpin>(
    mic: &mut S,
    mut finish: oneshot::Receiver<Finish>,
    limit: usize,
    mut level: impl FnMut(f32) -> Result<(), String>,
) -> Result<Option<Vec<f32>>, String> {
    let mut chunks = mic.ready_chunks(4096);
    let mut buf = Vec::new();
    loop {
        tokio::select! {
            f = &mut finish => return match f.expect("PushToTalk drops only senders whose receiver is gone") {
                Finish::Discard => Ok(None),
                Finish::Send if buf.is_empty() => Err("Nothing was recorded".into()),
                Finish::Send => Ok(Some(buf)),
            },
            c = chunks.next() => {
                let c = c.ok_or("Microphone stopped delivering audio")?;
                level(calculate_audio_metrics(&c).0)?;
                buf.extend(c);
                if buf.len() >= limit {
                    buf.truncate(limit);
                    return Ok(Some(buf));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Finish::{self, Discard, Send};
    use futures_util::stream::{self, StreamExt};
    use tokio::sync::oneshot;

    /// (samples fed before `finish`, finish, limit) -> recorded length, `None` = discarded
    type Case = (usize, Option<Finish>, usize, Result<Option<usize>, &'static str>);

    #[tokio::test]
    async fn record() {
        let cases: &[Case] = &[
            (100, Some(Send), 1000, Ok(Some(100))),
            (100, Some(Discard), 1000, Ok(None)),
            (0, Some(Send), 1000, Err("Nothing was recorded")),
            (5000, None, 1000, Ok(Some(1000))),
        ];
        for &(fed, action, limit, want) in cases {
            let (tx, rx) = oneshot::channel();
            let mut mic = stream::iter(vec![0.1f32; fed])
                .chain(stream::pending())
                .boxed();
            let mut levels = 0;
            let got = {
                let rec = super::record(&mut mic, rx, limit, |_| {
                    levels += 1;
                    Ok(())
                });
                tokio::pin!(rec);
                // drain what the mic has before finishing, like a user talking first
                match tokio::time::timeout(std::time::Duration::from_millis(50), &mut rec).await {
                    Ok(r) => r,
                    Err(_) => {
                        tx.send(action.expect("only limit cases end by themselves"))
                            .unwrap();
                        rec.await
                    }
                }
            };
            assert_eq!(
                got.map(|o| o.map(|b| b.len())),
                want.map_err(String::from),
                "{fed} {action:?}"
            );
            assert_eq!(levels > 0, fed > 0, "{fed} {action:?} levels");
        }
    }

    #[tokio::test]
    async fn ended_mic_is_an_error() {
        let (_tx, rx) = oneshot::channel();
        let mut mic = stream::iter(vec![0.1f32; 10]);
        let got = super::record(&mut mic, rx, 1000, |_| Ok(())).await;
        assert_eq!(got, Err("Microphone stopped delivering audio".into()));
    }
}
