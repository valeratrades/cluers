use super::AudioDevice;
use anyhow::{anyhow, bail, Result};
use futures_util::Stream;
use libpulse_binding as pulse;
use libpulse_simple_binding as psimple;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};
use std::thread;
use tracing::{error, warn};

use psimple::Simple;
use pulse::callbacks::ListResult;
use pulse::context::Context;
use pulse::def::BufferAttr;
use pulse::error::PAErr;
use pulse::mainloop::standard::{IterateResult, Mainloop};
use pulse::operation::{Operation, State as OperationState};
use pulse::sample::{Format, Spec};
use pulse::stream::Direction;
use pulse::time::MicroSeconds;

pub fn get_input_devices() -> Result<Vec<AudioDevice>> {
    list_devices(false)
}

pub fn get_output_devices() -> Result<Vec<AudioDevice>> {
    list_devices(true)
}

/// Sinks when `outputs`, otherwise non-monitor sources.
fn list_devices(outputs: bool) -> Result<Vec<AudioDevice>> {
    let mut pulse = Pulse::connect()?;
    let introspect = pulse.context.introspect();

    let default = Rc::new(RefCell::new(None));
    let d = default.clone();
    pulse.wait(introspect.get_server_info(move |info| {
        let name = match outputs {
            true => &info.default_sink_name,
            false => &info.default_source_name,
        };
        *d.borrow_mut() = name.as_deref().map(str::to_owned);
    }))?;

    let entries: Rc<RefCell<Vec<(Option<String>, Option<String>)>>> = Rc::default();
    let failed = Rc::new(Cell::new(false));
    let (e, f) = (entries.clone(), failed.clone());
    if outputs {
        pulse.wait(introspect.get_sink_info_list(move |r| match r {
            ListResult::Item(i) => e.borrow_mut().push((
                i.name.as_deref().map(str::to_owned),
                i.description.as_deref().map(str::to_owned),
            )),
            ListResult::End => {}
            ListResult::Error => f.set(true),
        }))?;
    } else {
        pulse.wait(introspect.get_source_info_list(move |r| match r {
            ListResult::Item(i) if i.monitor_of_sink.is_none() => e.borrow_mut().push((
                i.name.as_deref().map(str::to_owned),
                i.description.as_deref().map(str::to_owned),
            )),
            ListResult::Item(_) | ListResult::End => {}
            ListResult::Error => f.set(true),
        }))?;
    }
    if failed.get() {
        bail!("PulseAudio failed to list {}", if outputs { "sinks" } else { "sources" });
    }

    let default = default.take();
    Ok(entries
        .take()
        .into_iter()
        .map(|(name, description)| {
            let id = name.expect("PulseAudio sinks and sources are always named");
            AudioDevice {
                name: description.unwrap_or_else(|| id.clone()),
                is_default: default.as_ref() == Some(&id),
                id,
            }
        })
        .collect())
}

pub struct SpeakerInput {
    simple: Simple,
    sample_rate: u32,
}

impl SpeakerInput {
    pub fn new(device_id: Option<String>) -> Result<Self> {
        let source = match device_id {
            Some(id) if !id.is_empty() && id != "default" => format!("{id}.monitor"),
            _ => "@DEFAULT_MONITOR@".to_owned(),
        };
        // PipeWire-pulse silently records from the default source when the target is missing.
        let mut pulse = Pulse::connect()?;
        let found = Rc::new(Cell::new(false));
        let f = found.clone();
        let op = pulse
            .context
            .introspect()
            .get_source_info_by_name(&source, move |r| {
                if let ListResult::Item(_) = r {
                    f.set(true);
                }
            });
        pulse.wait(op)?;
        if !found.get() {
            bail!("PulseAudio source {source} not found");
        }

        // Pulse resamples to the requested rate, so this is what the stream delivers.
        let spec = Spec {
            format: Format::F32le,
            channels: 1,
            rate: 44_100,
        };
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: spec.usec_to_bytes(MicroSeconds(20_000)).try_into().unwrap(), // server default (~2s) lags VAD behind speech
        };
        let simple = Simple::new(
            None,
            "pluely",
            Direction::Record,
            Some(&source),
            "System Audio Capture",
            &spec,
            None,
            Some(&attr),
        )
        .map_err(|e| anyhow!("Failed to open PulseAudio source {source}: {e}"))?;
        Ok(Self {
            simple,
            sample_rate: spec.rate,
        })
    }

    pub fn stream(self) -> SpeakerStream {
        let shared = Arc::new(Mutex::new(Shared {
            queue: VecDeque::new(),
            waker: None,
            stop: false,
            error: None,
        }));
        let producer_shared = shared.clone();
        let simple = self.simple;
        let producer = thread::spawn(move || capture(simple, producer_shared));
        SpeakerStream {
            shared,
            producer: Some(producer),
            sample_rate: self.sample_rate,
        }
    }
}

struct Shared {
    queue: VecDeque<f32>,
    waker: Option<Waker>,
    stop: bool,
    error: Option<PAErr>,
}

fn capture(simple: Simple, shared: Arc<Mutex<Shared>>) {
    const MAX_QUEUED: usize = 131_072;
    let mut buffer = [0u8; 4096];
    while !shared.lock().unwrap().stop {
        let read = simple.read(&mut buffer);
        let mut s = shared.lock().unwrap();
        match read {
            Ok(()) => {
                s.queue.extend(
                    buffer
                        .chunks_exact(4)
                        .map(|c| f32::from_le_bytes(c.try_into().unwrap())),
                );
                if s.queue.len() > MAX_QUEUED {
                    let dropped = s.queue.len() - MAX_QUEUED;
                    s.queue.drain(..dropped);
                    warn!("[capture] Linux buffer overflow - dropped {dropped} samples");
                }
            }
            Err(e) => {
                error!("[capture] PulseAudio read error: {e}");
                s.error = Some(e);
            }
        }
        let failed = s.error.is_some();
        let waker = s.waker.take();
        drop(s);
        if let Some(w) = waker {
            w.wake();
        }
        if failed {
            return;
        }
    }
}

pub struct SpeakerStream {
    shared: Arc<Mutex<Shared>>,
    producer: Option<thread::JoinHandle<()>>,
    sample_rate: u32,
}

impl SpeakerStream {
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn error(&self) -> Option<anyhow::Error> {
        self.shared
            .lock()
            .unwrap()
            .error
            .map(|e| anyhow!("PulseAudio capture failed: {e}"))
    }
}

impl Drop for SpeakerStream {
    fn drop(&mut self) {
        self.shared.lock().unwrap().stop = true;
        if let Some(h) = self.producer.take() {
            h.join().expect("pulse capture thread panicked");
        }
    }
}

impl Stream for SpeakerStream {
    type Item = f32;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let mut s = self.shared.lock().unwrap();
        if let Some(sample) = s.queue.pop_front() {
            return Poll::Ready(Some(sample));
        }
        if s.error.is_some() {
            return Poll::Ready(None);
        }
        s.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

struct Pulse {
    context: Context, // must drop before `mainloop`
    mainloop: Mainloop,
}

impl Pulse {
    fn connect() -> Result<Self> {
        let mainloop =
            Mainloop::new().ok_or_else(|| anyhow!("Failed to create PulseAudio mainloop"))?;
        let mut context = Context::new(&mainloop, "pluely")
            .ok_or_else(|| anyhow!("Failed to create PulseAudio context"))?;
        context
            .connect(None, pulse::context::FlagSet::NOFLAGS, None)
            .map_err(|e| anyhow!("Failed to connect to PulseAudio: {e}"))?;
        let mut this = Self { context, mainloop };
        loop {
            this.iterate()?;
            match this.context.get_state() {
                pulse::context::State::Ready => return Ok(this),
                pulse::context::State::Failed | pulse::context::State::Terminated => {
                    return Err(anyhow!("PulseAudio context failed"))
                }
                _ => {}
            }
        }
    }

    fn iterate(&mut self) -> Result<()> {
        match self.mainloop.iterate(true) {
            IterateResult::Success(_) => Ok(()),
            IterateResult::Quit(r) => Err(anyhow!("PulseAudio mainloop quit ({})", r.0)),
            IterateResult::Err(e) => Err(anyhow!("PulseAudio mainloop failed: {e}")),
        }
    }

    fn wait<G: ?Sized>(&mut self, op: Operation<G>) -> Result<()> {
        loop {
            match op.get_state() {
                OperationState::Running => self.iterate()?,
                OperationState::Done => return Ok(()),
                OperationState::Cancelled => {
                    return Err(anyhow!("PulseAudio operation cancelled"))
                }
            }
        }
    }
}

impl Drop for Pulse {
    fn drop(&mut self) {
        self.context.disconnect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speaker::SpeakerInput;
    use futures_util::StreamExt;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    /// Private sink per test, so tests never touch the user's real devices.
    struct NullSink {
        name: String,
        module: u32,
    }

    impl NullSink {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let name = format!(
                "cluers_test_{}_{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            );
            let mut pulse =
                Pulse::connect().expect("tests need a running Pulse-compatible server");
            let module = Rc::new(Cell::new(pulse::def::INVALID_INDEX));
            let m = module.clone();
            let op = pulse.context.introspect().load_module(
                "module-null-sink",
                &format!("sink_name={name}"),
                move |idx| m.set(idx),
            );
            pulse.wait(op).unwrap();
            assert_ne!(module.get(), pulse::def::INVALID_INDEX, "load module-null-sink");
            Self {
                name,
                module: module.get(),
            }
        }

        fn monitor(&self) -> String {
            format!("{}.monitor", self.name)
        }
    }

    impl Drop for NullSink {
        fn drop(&mut self) {
            let mut pulse = Pulse::connect().unwrap();
            let ok = Rc::new(Cell::new(false));
            let o = ok.clone();
            let op = pulse
                .context
                .introspect()
                .unload_module(self.module, move |s| o.set(s));
            pulse.wait(op).unwrap();
            assert!(ok.get(), "unload module {}", self.module);
        }
    }

    fn kill_captures_of(source_name: &str) {
        let mut pulse = Pulse::connect().unwrap();
        let mut introspect = pulse.context.introspect();

        let source = Rc::new(Cell::new(None));
        let s = source.clone();
        let op = introspect.get_source_info_by_name(source_name, move |r| {
            if let ListResult::Item(i) = r {
                s.set(Some(i.index));
            }
        });
        pulse.wait(op).unwrap();
        let source = source.get().expect("monitor source exists");

        let outputs = Rc::new(RefCell::new(Vec::new()));
        let o = outputs.clone();
        let op = introspect.get_source_output_info_list(move |r| {
            if let ListResult::Item(i) = r {
                if i.source == source {
                    o.borrow_mut().push(i.index);
                }
            }
        });
        pulse.wait(op).unwrap();
        let outputs = outputs.take();
        assert!(!outputs.is_empty(), "capture stream attached to {source_name}");

        for idx in outputs {
            let ok = Rc::new(Cell::new(false));
            let k = ok.clone();
            let op = introspect.kill_source_output(idx, move |s| k.set(s));
            pulse.wait(op).unwrap();
            assert!(ok.get(), "kill source output {idx}");
        }
    }

    #[test]
    fn new_errors_for_missing_device() {
        assert!(SpeakerInput::new_with_device(Some("cluers-no-such-sink".into())).is_err());
    }

    #[test]
    fn new_opens_default_monitor() {
        SpeakerInput::new().unwrap();
    }

    #[tokio::test]
    async fn stream_ends_with_error_when_capture_is_killed() {
        let sink = NullSink::new();
        let mut stream = SpeakerInput::new_with_device(Some(sink.name.clone()))
            .unwrap()
            .stream();

        tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("capture delivers samples")
            .expect("stream alive");

        kill_captures_of(&sink.monitor());

        tokio::time::timeout(Duration::from_secs(5), async {
            while stream.next().await.is_some() {}
        })
        .await
        .expect("stream ends after its capture is killed");
        assert!(stream.error().is_some());
    }

    #[tokio::test]
    async fn first_sample_arrives_within_200ms() {
        let sink = NullSink::new();
        let started = std::time::Instant::now();
        let mut stream = SpeakerInput::new_with_device(Some(sink.name.clone()))
            .unwrap()
            .stream();
        stream.next().await.expect("stream alive");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(200),
            "first sample after {elapsed:?}"
        );
    }

    #[test]
    fn null_sink_listed_as_output_only() {
        let sink = NullSink::new();
        assert!(get_output_devices()
            .unwrap()
            .iter()
            .any(|d| d.id == sink.name));
        assert!(!get_input_devices()
            .unwrap()
            .iter()
            .any(|d| d.id == sink.monitor()));
    }
}
