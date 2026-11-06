//! Native audio on a dedicated thread. Capture is opt-in; never starts on launch.
use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::{
    io::Cursor,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

pub struct Recorder {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<Result<Vec<u8>>>>,
}
impl Recorder {
    pub fn start() -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let (ready, started) = std::sync::mpsc::sync_channel(1);
        let thread = thread::spawn(move || {
            let result = (|| -> Result<Vec<u8>> {
                let device = cpal::default_host()
                    .default_input_device()
                    .context("microphone not available")?;
                let config = device.default_input_config()?;
                let rate = config.sample_rate().0;
                let channels = config.channels() as usize;
                let samples = Arc::new(Mutex::new(Vec::<f32>::new()));
                let cap = rate as usize * 30;
                macro_rules! capture {
                    ($ty:ty) => {{
                        let buffer = samples.clone();
                        let flag = stopped.clone();
                        device.build_input_stream(
                            &config.clone().into(),
                            move |data: &[$ty], _| {
                                if flag.load(Ordering::Relaxed) {
                                    return;
                                }
                                if let Ok(mut out) = buffer.lock() {
                                    for frame in data.chunks(channels) {
                                        if out.len() >= cap {
                                            flag.store(true, Ordering::Relaxed);
                                            break;
                                        }
                                        let v = frame
                                            .iter()
                                            .map(|s| cpal::Sample::to_sample::<f32>(*s))
                                            .sum::<f32>()
                                            / channels as f32;
                                        out.push(v);
                                    }
                                }
                            },
                            move |_| {},
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
                stream.play()?;
                let _ = ready.send(Ok(()));
                while !stopped.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(10));
                }
                drop(stream);
                let samples = samples
                    .lock()
                    .map_err(|_| anyhow::anyhow!("capture buffer poisoned"))?;
                encode_wav(&samples, rate)
            })();
            if let Err(error) = &result {
                let _ = ready.try_send(Err(error.to_string()));
            }
            result
        });
        match started.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {}
            result => {
                stop.store(true, Ordering::Relaxed);
                match result {
                    Ok(Err(error)) => bail!(error),
                    _ => bail!("microphone startup timed out"),
                }
            }
        }
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
    pub fn finish(mut self) -> Result<Vec<u8>> {
        self.stop.store(true, Ordering::Relaxed);
        self.thread
            .take()
            .context("capture already finished")?
            .join()
            .map_err(|_| anyhow::anyhow!("capture thread failed"))?
    }
}
impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
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
}
impl PlaybackController {
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
        macro_rules! output {
            ($ty:ty) => {{
                let samples = samples.clone();
                let cursor = position.clone();
                let drain_due=drain_due.clone();
                let stream_error=stream_error.clone();
                let current = self.epoch.clone();
                device.build_output_stream(
                    &config.clone().into(),
                    move |data: &mut [$ty], info:&cpal::OutputCallbackInfo| {
                        let frames=data.len()/channels;
                        let mut tail=false;
                        for frame in data.chunks_mut(channels) {
                            let index = cursor.fetch_add(1, Ordering::Relaxed) * source_rate as u64
                                / output_rate as u64;
                            let value = if current.load(Ordering::SeqCst) != epoch {
                                0.0
                            } else if let Some(value) = samples.get(index as usize) {
                                *value
                            } else {
                                tail=true;
                                0.0
                            };
                            for sample in frame {
                                *sample = <$ty as cpal::FromSample<f32>>::from_sample_(value);
                            }
                        }
                        if tail{
                            // The callback fills a future device buffer; it is not playback completion.
                            let timestamp=info.timestamp();
                            let device_delay=timestamp.playback.duration_since(&timestamp.callback).unwrap_or_default();
                            let due=anchor.elapsed()+device_delay+Duration::from_secs_f64(frames as f64/output_rate as f64);
                            let _=drain_due.compare_exchange(0,due.as_nanos().min(u64::MAX as u128)as u64,Ordering::Release,Ordering::Relaxed);
                        }
                    },
                    move |_| {stream_error.store(true,Ordering::Release);},
                    None,
                )?
            }};
        }
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => output!(f32),
            cpal::SampleFormat::I16 => output!(i16),
            cpal::SampleFormat::U16 => output!(u16),
            _ => bail!("unsupported output format"),
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
    fn wav_is_bounded_mono_and_cancel_invalidates_queued_audio() {
        let wav = encode_wav(&vec![0.1; 48000], 48000).unwrap();
        let reader = hound::WavReader::new(Cursor::new(wav)).unwrap();
        assert_eq!(reader.spec().sample_rate, 16000);
        assert_eq!(reader.duration(), 16000);
        let p = PlaybackController::default();
        let epoch = p.epoch();
        p.stop();
        assert!(p.epoch() != epoch);
        assert!(!p.play(vec![], epoch).unwrap());
    }
}
