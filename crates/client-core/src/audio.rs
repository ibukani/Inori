//! Native audio capture, voice activity detection, echo cancellation and playback.
use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use earshot::Detector;
use sonora::{
    AudioProcessing, Config as ProcessingConfig, StreamConfig as ProcessingStreamConfig,
    config::{EchoCanceller, NoiseSuppression},
};
use std::{
    collections::VecDeque,
    io::Cursor,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{SyncSender, sync_channel},
    },
    thread,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

const VAD_FRAME_SAMPLES: usize = 256;
const PROCESSING_FRAME_SAMPLES: usize = 160;
const SPEECH_START_SCORE: f32 = 0.5;
const SPEECH_CONTINUE_SCORE: f32 = 0.35;
const SPEECH_START_FRAMES: usize = 2;
const SILENCE_END_FRAMES: usize = 32;
const PRE_ROLL_SAMPLES: usize = VAD_FRAME_SAMPLES * 4;
const MAX_SEGMENT_SAMPLES: usize = 16_000 * 30;
const MIN_SEGMENT_SAMPLES: usize = 1_600;

#[derive(Debug)]
pub enum VoiceCaptureEvent {
    SpeechStarted { epoch: u64 },
    Segment { epoch: u64, wav: Vec<u8> },
    Error { epoch: u64, message: String },
    Stopped { epoch: u64 },
}

#[derive(Clone, Copy)]
enum AudioFrameKind {
    Capture,
    Render,
}

struct AudioFrame {
    kind: AudioFrameKind,
    samples: Vec<f32>,
    callback_delay: Duration,
    queued_at: Instant,
}

#[derive(Clone)]
struct FrameBridge {
    tx: SyncSender<AudioFrame>,
    overrun: Arc<AtomicBool>,
}

impl FrameBridge {
    fn send(&self, frame: AudioFrame) {
        if self.tx.try_send(frame).is_err() {
            self.overrun.store(true, Ordering::Release);
        }
    }
}

/// Starts hands-free capture. Audio remains in memory and only completed speech segments
/// are sent to the configured server.
pub struct VoiceCapture {
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl VoiceCapture {
    pub fn start(
        playback: PlaybackController,
        events: mpsc::Sender<VoiceCaptureEvent>,
        epoch: u64,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let (ready, started) = std::sync::mpsc::sync_channel(1);
        let ready_error = ready.clone();
        let worker = thread::spawn(move || {
            let run = (|| -> Result<()> {
                let device = cpal::default_host()
                    .default_input_device()
                    .context("microphone not available")?;
                let config = device.default_input_config()?;
                let rate = config.sample_rate().0;
                let channels = config.channels() as usize;
                if channels == 0 || rate == 0 {
                    bail!("microphone returned an invalid audio format");
                }
                let (frames_tx, frames_rx) = sync_channel(128);
                let overrun = Arc::new(AtomicBool::new(false));
                let bridge = FrameBridge {
                    tx: frames_tx,
                    overrun: overrun.clone(),
                };
                let stream_error = Arc::new(AtomicBool::new(false));
                macro_rules! capture {
                    ($ty:ty) => {{
                        let sender = bridge.clone();
                        let failed = stream_error.clone();
                        let mut resampler = StreamingResampler::new(rate);
                        device.build_input_stream(
                            &config.clone().into(),
                            move |data: &[$ty], info: &cpal::InputCallbackInfo| {
                                let mut mono = Vec::with_capacity(data.len() / channels);
                                for frame in data.chunks(channels) {
                                    let sample = frame
                                        .iter()
                                        .map(|value| cpal::Sample::to_sample::<f32>(*value))
                                        .sum::<f32>()
                                        / channels as f32;
                                    mono.push(sample);
                                }
                                let samples = resampler.push(&mono);
                                if !samples.is_empty() {
                                    let timestamp = info.timestamp();
                                    sender.send(AudioFrame {
                                        kind: AudioFrameKind::Capture,
                                        samples,
                                        callback_delay: timestamp
                                            .callback
                                            .duration_since(&timestamp.capture)
                                            .unwrap_or_default(),
                                        queued_at: Instant::now(),
                                    });
                                }
                            },
                            move |_| failed.store(true, Ordering::Release),
                            None,
                        )?
                    }};
                }
                let stream = match config.sample_format() {
                    cpal::SampleFormat::F32 => capture!(f32),
                    cpal::SampleFormat::I16 => capture!(i16),
                    cpal::SampleFormat::U16 => capture!(u16),
                    _ => bail!("unsupported microphone format"),
                };
                playback.set_render_bridge(Some(bridge));
                stream.play()?;
                let _ = ready.send(Ok(()));

                let mut processor = AudioProcessor::new(overrun);
                while !worker_stop.load(Ordering::Acquire) {
                    match frames_rx.recv_timeout(Duration::from_millis(15)) {
                        Ok(frame) => processor.process(frame, epoch, &events),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    if stream_error.swap(false, Ordering::AcqRel) {
                        let _ = events.try_send(VoiceCaptureEvent::Error {
                            epoch,
                            message: "マイクのストリームが停止しました。".into(),
                        });
                        break;
                    }
                }
                drop(stream);
                playback.set_render_bridge(None);
                if !worker_stop.load(Ordering::Acquire) {
                    let _ = events.try_send(VoiceCaptureEvent::Stopped { epoch });
                }
                Ok(())
            })();
            if let Err(error) = run {
                let message = error.to_string();
                let _ = ready_error.try_send(Err(anyhow::anyhow!(message.clone())));
                let _ = events.try_send(VoiceCaptureEvent::Error { epoch, message });
            }
        });

        match started.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                stop,
                worker: Some(worker),
            }),
            Ok(Err(error)) => {
                stop.store(true, Ordering::Release);
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                stop.store(true, Ordering::Release);
                let _ = worker.join();
                bail!("microphone startup timed out");
            }
        }
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for VoiceCapture {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct StreamingResampler {
    source_rate: f64,
    input: VecDeque<f32>,
    base_index: u64,
    total_input: u64,
    next_position: f64,
}

impl StreamingResampler {
    fn new(source_rate: u32) -> Self {
        Self {
            source_rate: source_rate as f64,
            input: VecDeque::new(),
            base_index: 0,
            total_input: 0,
            next_position: 0.0,
        }
    }

    fn push(&mut self, samples: &[f32]) -> Vec<f32> {
        self.input.extend(samples.iter().copied());
        self.total_input += samples.len() as u64;
        let step = self.source_rate / 16_000.0;
        let mut output =
            Vec::with_capacity(((samples.len() as f64 / step).ceil() as usize).saturating_add(1));
        while self.next_position + 1.0 < self.total_input as f64 {
            let left_index = self.next_position.floor() as u64;
            let fraction = (self.next_position - left_index as f64) as f32;
            let left = self.input[(left_index - self.base_index) as usize];
            let right = self.input[(left_index + 1 - self.base_index) as usize];
            output.push(left + (right - left) * fraction);
            self.next_position += step;
        }
        let keep_from = self.next_position.floor() as u64;
        while self.base_index < keep_from && self.input.len() > 1 {
            self.input.pop_front();
            self.base_index += 1;
        }
        output
    }
}

struct AudioProcessor {
    apm: AudioProcessing,
    vad: Detector,
    render_pending: VecDeque<f32>,
    capture_pending: VecDeque<f32>,
    vad_pending: VecDeque<f32>,
    last_render_delay: Duration,
    segmenter: SpeechSegmenter,
    overrun: Arc<AtomicBool>,
}

impl AudioProcessor {
    fn new(overrun: Arc<AtomicBool>) -> Self {
        let config = ProcessingConfig {
            echo_canceller: Some(EchoCanceller::default()),
            noise_suppression: Some(NoiseSuppression::default()),
            ..Default::default()
        };
        let apm = AudioProcessing::builder()
            .config(config)
            .capture_config(ProcessingStreamConfig::new(16_000, 1))
            .render_config(ProcessingStreamConfig::new(16_000, 1))
            .build();
        Self {
            apm,
            vad: Detector::default(),
            render_pending: VecDeque::new(),
            capture_pending: VecDeque::new(),
            vad_pending: VecDeque::new(),
            last_render_delay: Duration::ZERO,
            segmenter: SpeechSegmenter::default(),
            overrun,
        }
    }

    fn process(&mut self, frame: AudioFrame, epoch: u64, events: &mpsc::Sender<VoiceCaptureEvent>) {
        if self.overrun.swap(false, Ordering::AcqRel) {
            self.segmenter.reset();
            self.vad.reset();
            self.capture_pending.clear();
            self.vad_pending.clear();
            let _ = events.try_send(VoiceCaptureEvent::Error {
                epoch,
                message: "音声処理が混雑したため、発話区間をリセットしました。".into(),
            });
        }
        match frame.kind {
            AudioFrameKind::Render => {
                self.last_render_delay = frame.callback_delay;
                self.render_pending.extend(frame.samples);
                while self.render_pending.len() >= PROCESSING_FRAME_SAMPLES {
                    let source = take_frame(&mut self.render_pending);
                    let mut output = [0.0; PROCESSING_FRAME_SAMPLES];
                    let _ = self.apm.process_render_f32(&[&source], &mut [&mut output]);
                }
            }
            AudioFrameKind::Capture => {
                let delay = self
                    .last_render_delay
                    .saturating_add(frame.callback_delay)
                    .saturating_add(frame.queued_at.elapsed())
                    .as_millis()
                    .min(500) as i32;
                let _ = self.apm.set_stream_delay_ms(delay);
                self.capture_pending.extend(frame.samples);
                while self.capture_pending.len() >= PROCESSING_FRAME_SAMPLES {
                    let source = take_frame(&mut self.capture_pending);
                    let mut output = [0.0; PROCESSING_FRAME_SAMPLES];
                    if self
                        .apm
                        .process_capture_f32(&[&source], &mut [&mut output])
                        .is_err()
                    {
                        continue;
                    }
                    self.vad_pending.extend(output);
                    while self.vad_pending.len() >= VAD_FRAME_SAMPLES {
                        let frame = take_vad_frame(&mut self.vad_pending);
                        let score = self.vad.predict_f32(&frame);
                        let (started, completed) = self.segmenter.push(&frame, score);
                        if started {
                            let _ = events.try_send(VoiceCaptureEvent::SpeechStarted { epoch });
                        }
                        if let Some(samples) = completed
                            && let Ok(wav) = encode_wav(&samples, 16_000)
                        {
                            let _ = events.try_send(VoiceCaptureEvent::Segment { epoch, wav });
                        }
                    }
                }
            }
        }
    }
}

fn take_frame(samples: &mut VecDeque<f32>) -> [f32; PROCESSING_FRAME_SAMPLES] {
    let mut frame = [0.0; PROCESSING_FRAME_SAMPLES];
    for sample in &mut frame {
        *sample = samples.pop_front().unwrap_or_default();
    }
    frame
}

fn take_vad_frame(samples: &mut VecDeque<f32>) -> [f32; VAD_FRAME_SAMPLES] {
    let mut frame = [0.0; VAD_FRAME_SAMPLES];
    for sample in &mut frame {
        *sample = samples.pop_front().unwrap_or_default();
    }
    frame
}

#[derive(Default)]
struct SpeechSegmenter {
    pre_roll: VecDeque<f32>,
    utterance: Vec<f32>,
    start_streak: usize,
    silence_streak: usize,
    active: bool,
}

impl SpeechSegmenter {
    fn reset(&mut self) {
        self.pre_roll.clear();
        self.utterance.clear();
        self.start_streak = 0;
        self.silence_streak = 0;
        self.active = false;
    }

    fn push(&mut self, frame: &[f32; VAD_FRAME_SAMPLES], score: f32) -> (bool, Option<Vec<f32>>) {
        if self.active {
            self.utterance.extend_from_slice(frame);
            if score < SPEECH_CONTINUE_SCORE {
                self.silence_streak += 1;
            } else if score >= SPEECH_START_SCORE {
                self.silence_streak = 0;
            }
            if self.silence_streak >= SILENCE_END_FRAMES
                || self.utterance.len() >= MAX_SEGMENT_SAMPLES
            {
                return (false, self.finish());
            }
            return (false, None);
        }

        if score >= SPEECH_START_SCORE {
            self.start_streak += 1;
            if self.start_streak >= SPEECH_START_FRAMES {
                self.active = true;
                self.silence_streak = 0;
                self.utterance.extend(self.pre_roll.drain(..));
                self.utterance.extend_from_slice(frame);
                return (true, None);
            }
        } else {
            self.start_streak = 0;
        }
        self.pre_roll.extend(frame.iter().copied());
        while self.pre_roll.len() > PRE_ROLL_SAMPLES {
            self.pre_roll.pop_front();
        }
        (false, None)
    }

    fn finish(&mut self) -> Option<Vec<f32>> {
        self.active = false;
        self.start_streak = 0;
        self.silence_streak = 0;
        self.pre_roll.clear();
        let utterance = std::mem::take(&mut self.utterance);
        (utterance.len() >= MIN_SEGMENT_SAMPLES).then_some(utterance)
    }
}

pub fn encode_wav(samples: &[f32], rate: u32) -> Result<Vec<u8>> {
    if rate == 0 || samples.is_empty() {
        bail!("recording is empty");
    }
    let length = samples.len() as u64 * 16000 / rate as u64;
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut writer = hound::WavWriter::new(
            &mut cursor,
            hound::WavSpec {
                channels: 1,
                sample_rate: 16000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )?;
        for i in 0..length {
            let at = i as f64 * rate as f64 / 16000.0;
            let index = at as usize;
            let next = (index + 1).min(samples.len() - 1);
            let sample =
                samples[index] + (samples[next] - samples[index]) * (at - index as f64) as f32;
            writer.write_sample((sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
        }
        writer.finalize()?;
    }
    Ok(cursor.into_inner())
}

#[derive(Clone, Default)]
pub struct PlaybackController {
    epoch: Arc<AtomicU64>,
    render_bridge: Arc<Mutex<Option<FrameBridge>>>,
}

impl PlaybackController {
    fn set_render_bridge(&self, bridge: Option<FrameBridge>) {
        if let Ok(mut current) = self.render_bridge.lock() {
            *current = bridge;
        }
    }

    pub fn stop(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    pub fn play(&self, bytes: Vec<u8>, epoch: u64) -> Result<bool> {
        if self.epoch() != epoch {
            return Ok(false);
        }
        let mut wav = hound::WavReader::new(Cursor::new(bytes))?;
        let spec = wav.spec();
        let source_rate = spec.sample_rate;
        let source_channels = spec.channels as usize;
        let raw: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Float => wav
                .samples::<f32>()
                .collect::<std::result::Result<Vec<_>, _>>()?,
            hound::SampleFormat::Int => wav
                .samples::<i32>()
                .map(|s| s.map(|s| s as f32 / (2f32.powi(spec.bits_per_sample as i32 - 1))))
                .collect::<std::result::Result<Vec<_>, _>>()?,
        };
        if source_channels == 0 || source_rate == 0 {
            bail!("invalid WAV format");
        }
        let samples: Arc<Vec<f32>> = Arc::new(
            raw.chunks(source_channels)
                .map(|f| f.iter().sum::<f32>() / source_channels as f32)
                .collect(),
        );
        if samples.is_empty() {
            bail!("speech engine returned empty audio");
        }
        let device = cpal::default_host()
            .default_output_device()
            .context("speaker not available")?;
        let config = device.default_output_config()?;
        let output_rate = config.sample_rate().0;
        let channels = config.channels() as usize;
        let position = Arc::new(AtomicU64::new(0));
        let anchor = Instant::now();
        let drain_due = Arc::new(AtomicU64::new(0));
        let stream_error = Arc::new(AtomicBool::new(false));
        let render_bridge = self.render_bridge.clone();
        macro_rules! output {
            ($ty:ty) => {{
                let samples = samples.clone();
                let cursor = position.clone();
                let drain_due = drain_due.clone();
                let stream_error = stream_error.clone();
                let current = self.epoch.clone();
                let render_bridge = render_bridge.clone();
                let mut resampler = StreamingResampler::new(output_rate);
                device.build_output_stream(
                    &config.clone().into(),
                    move |data: &mut [$ty], info: &cpal::OutputCallbackInfo| {
                        let frames = data.len() / channels;
                        let mut rendered = Vec::with_capacity(frames);
                        let mut tail = false;
                        for frame in data.chunks_mut(channels) {
                            let index = cursor.fetch_add(1, Ordering::Relaxed) * source_rate as u64
                                / output_rate as u64;
                            let value = if current.load(Ordering::SeqCst) != epoch {
                                0.0
                            } else if let Some(value) = samples.get(index as usize) {
                                *value
                            } else {
                                tail = true;
                                0.0
                            };
                            rendered.push(value);
                            for sample in frame {
                                *sample = <$ty as cpal::FromSample<f32>>::from_sample_(value);
                            }
                        }
                        if let Ok(reference) = render_bridge.try_lock() {
                            if let Some(reference) = reference.clone() {
                                let samples = resampler.push(&rendered);
                                if !samples.is_empty() {
                                    let timestamp = info.timestamp();
                                    reference.send(AudioFrame {
                                        kind: AudioFrameKind::Render,
                                        samples,
                                        callback_delay: timestamp
                                            .playback
                                            .duration_since(&timestamp.callback)
                                            .unwrap_or_default(),
                                        queued_at: Instant::now(),
                                    });
                                }
                            }
                        }
                        if tail {
                            // The callback fills a future device buffer; it is not playback completion.
                            let timestamp = info.timestamp();
                            let device_delay = timestamp
                                .playback
                                .duration_since(&timestamp.callback)
                                .unwrap_or_default();
                            let due = anchor.elapsed()
                                + device_delay
                                + Duration::from_secs_f64(frames as f64 / output_rate as f64);
                            let _ = drain_due.compare_exchange(
                                0,
                                due.as_nanos().min(u64::MAX as u128) as u64,
                                Ordering::Release,
                                Ordering::Relaxed,
                            );
                        }
                    },
                    move |_| stream_error.store(true, Ordering::Release),
                    None,
                )?
            }};
        }
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => output!(f32),
            cpal::SampleFormat::I16 => output!(i16),
            cpal::SampleFormat::U16 => output!(u16),
            _ => bail!("unsupported speaker format"),
        };
        stream.play()?;
        let deadline = Instant::now()
            + Duration::from_secs_f64(samples.len() as f64 / source_rate as f64 + 2.0);
        let mut completed = false;
        while self.epoch() == epoch && Instant::now() < deadline {
            if stream_error.load(Ordering::Acquire) {
                break;
            }
            let due = drain_due.load(Ordering::Acquire);
            if due > 0 && anchor.elapsed().as_nanos() >= due as u128 {
                completed = true;
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        drop(stream);
        Ok(completed && self.epoch() == epoch && !stream_error.load(Ordering::Acquire))
    }
}

pub fn devices() -> Result<serde_json::Value> {
    let host = cpal::default_host();
    let input = host.default_input_device().map(|d| d.name()).transpose()?;
    let output = host.default_output_device().map(|d| d.name()).transpose()?;
    Ok(serde_json::json!({"input":input,"output":output,"capture_started":false}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_is_mono_16khz_and_playback_cancel_invalidates_audio() {
        let wav = encode_wav(&vec![0.1; 48_000], 48_000).unwrap();
        let reader = hound::WavReader::new(Cursor::new(wav)).unwrap();
        assert_eq!(reader.spec().sample_rate, 16_000);
        assert_eq!(reader.duration(), 16_000);
        let playback = PlaybackController::default();
        let epoch = playback.epoch();
        playback.stop();
        assert_ne!(playback.epoch(), epoch);
        assert!(!playback.play(vec![], epoch).unwrap());
    }

    #[test]
    fn resampler_keeps_streaming_phase_across_input_chunks() {
        let mut resampler = StreamingResampler::new(48_000);
        let first = resampler.push(&vec![0.25; 480]);
        let second = resampler.push(&vec![0.25; 480]);
        assert_eq!(first.len() + second.len(), 320);
        assert!(
            first
                .iter()
                .chain(&second)
                .all(|sample| (*sample - 0.25).abs() < 0.001)
        );
    }

    #[test]
    fn voice_segmenter_ignores_silence_and_finishes_after_speech_pause() {
        let frame = [0.1; VAD_FRAME_SAMPLES];
        let mut segmenter = SpeechSegmenter::default();
        for _ in 0..40 {
            assert!(segmenter.push(&frame, 0.0).1.is_none());
        }
        assert!(segmenter.push(&frame, SPEECH_START_SCORE).1.is_none());
        let (started, _) = segmenter.push(&frame, SPEECH_START_SCORE);
        assert!(started);
        for _ in 0..SILENCE_END_FRAMES - 1 {
            assert!(segmenter.push(&frame, 0.0).1.is_none());
        }
        let completed = segmenter.push(&frame, 0.0).1.unwrap();
        assert!(completed.len() >= MIN_SEGMENT_SAMPLES);
        let wav = encode_wav(&completed, 16_000).unwrap();
        let reader = hound::WavReader::new(Cursor::new(wav)).unwrap();
        assert_eq!(reader.spec().sample_rate, 16_000);
    }

    #[test]
    fn voice_segmenter_splits_long_speech_at_its_memory_bound() {
        let frame = [0.1; VAD_FRAME_SAMPLES];
        let mut segmenter = SpeechSegmenter::default();
        segmenter.push(&frame, SPEECH_START_SCORE);
        segmenter.push(&frame, SPEECH_START_SCORE);
        let frames = MAX_SEGMENT_SAMPLES / VAD_FRAME_SAMPLES;
        let mut completed = None;
        for _ in 2..frames {
            completed = segmenter.push(&frame, SPEECH_START_SCORE).1.or(completed);
        }
        assert!(completed.is_some());
        assert!(completed.unwrap().len() <= MAX_SEGMENT_SAMPLES);
    }

    #[test]
    fn muting_resets_and_discards_an_unsubmitted_utterance() {
        let frame = [0.1; VAD_FRAME_SAMPLES];
        let mut segmenter = SpeechSegmenter::default();
        segmenter.push(&frame, SPEECH_START_SCORE);
        assert!(segmenter.push(&frame, SPEECH_START_SCORE).0);
        segmenter.push(&frame, SPEECH_START_SCORE);

        segmenter.reset();

        for _ in 0..SILENCE_END_FRAMES + 1 {
            assert!(segmenter.push(&frame, 0.0).1.is_none());
        }
        assert!(segmenter.utterance.is_empty());
        assert!(!segmenter.active);
    }

    #[test]
    fn earshot_rejects_silence_and_detects_a_synthetic_voiced_signal() {
        let mut detector = Detector::default();
        let silence = [0.0; VAD_FRAME_SAMPLES];
        let mut silence_max: f32 = 0.0;
        for _ in 0..60 {
            silence_max = silence_max.max(detector.predict_f32(&silence));
        }

        let mut phase = 0.0_f32;
        let mut voice_max: f32 = 0.0;
        let frames = (0..120).map(|frame_index| {
            let mut frame = [0.0; VAD_FRAME_SAMPLES];
            for (sample_index, sample) in frame.iter_mut().enumerate() {
                let t = (frame_index * VAD_FRAME_SAMPLES + sample_index) as f32 / 16_000.0;
                let fundamental = 118.0 + 12.0 * (std::f32::consts::TAU * 2.1 * t).sin();
                phase += std::f32::consts::TAU * fundamental / 16_000.0;
                let envelope = 0.2 + 0.08 * (std::f32::consts::TAU * 3.0 * t).sin().abs();
                *sample = envelope
                    * (phase.sin()
                        + 0.55 * (phase * 2.0).sin()
                        + 0.28 * (phase * 3.0).sin()
                        + 0.14 * (phase * 4.0).sin());
            }
            frame
        });
        for frame in frames {
            voice_max = voice_max.max(detector.predict_f32(&frame));
        }

        assert!(silence_max < SPEECH_START_SCORE);
        assert!(
            voice_max >= SPEECH_START_SCORE,
            "synthetic voice score: {voice_max}"
        );
    }

    #[test]
    fn audio_processor_does_not_emit_segments_for_silence() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut processor = AudioProcessor::new(Arc::new(AtomicBool::new(false)));
        for _ in 0..100 {
            processor.process(
                AudioFrame {
                    kind: AudioFrameKind::Capture,
                    samples: vec![0.0; PROCESSING_FRAME_SAMPLES],
                    callback_delay: Duration::ZERO,
                    queued_at: Instant::now(),
                },
                1,
                &tx,
            );
        }
        assert!(rx.try_recv().is_err());
    }
}
