//! Приём звука с микрофона и нарезка на фразы.
//!
//! Модуль не распознаёт речь и не открывает сеть. На выходе — моно 16 кГц,
//! готовое для распознавателя. Контракт описан в `docs/modules/audio-capture.md`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SizedSample};

pub const OUTPUT_RATE: u32 = 16_000;

#[derive(Debug, Clone)]
pub struct Config {
    /// Среднеквадратичный уровень кадра, с которого кадр считается речью.
    pub threshold: f32,
    /// Сколько тишины закрывает фразу.
    pub silence_ms: u32,
    /// Короче этого речь выбрасывается как щелчок.
    pub min_speech_ms: u32,
    /// Звук до порога, чтобы не срезать первую согласную.
    pub preroll_ms: u32,
    /// Жёсткий потолок одной фразы. Дальше режем, даже если человек ещё говорит.
    pub max_phrase_ms: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            threshold: 0.02,
            silence_ms: 700,
            min_speech_ms: 300,
            preroll_ms: 200,
            max_phrase_ms: 30_000,
        }
    }
}

#[derive(Debug)]
pub struct DeviceInfo {
    pub name: String,
    pub sample_rate: u32,
    pub channels: u16,
}

/// Одна законченная фраза. Сэмплы моно, `sample_rate` всегда [`OUTPUT_RATE`].
#[derive(Debug, Clone)]
pub struct Phrase {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub peak: f32,
    pub duration_ms: u32,
}

#[derive(Debug)]
pub enum Event {
    Ready(DeviceInfo),
    /// Раз в секунду, пока фраза не идёт. `rms` — уровень последнего кадра.
    Listening { rms: f32 },
    SpeechStarted,
    Phrase(Phrase),
}

#[derive(Debug)]
pub struct Probe {
    pub device_name: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub seconds: f32,
    pub rms: f32,
    pub peak: f32,
}

#[derive(Debug)]
pub enum CaptureError {
    NoInputDevice,
    UnsupportedFormat(String),
    Device(String),
    NoAudio,
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoInputDevice => write!(f, "входное устройство не найдено"),
            Self::UnsupportedFormat(format) => {
                write!(f, "формат микрофона {format} не поддерживается")
            }
            Self::Device(message) => write!(f, "микрофон: {message}"),
            Self::NoAudio => write!(
                f,
                "микрофон не отдал звук. Проверьте разрешение в Настройках системы → Конфиденциальность и безопасность → Микрофон"
            ),
        }
    }
}

impl std::error::Error for CaptureError {}

/// Открывает микрофон по умолчанию, читает `seconds` секунд и возвращает уровень.
/// Сюда не входит нарезка фраз.
pub fn probe(seconds: f32) -> Result<Probe, CaptureError> {
    let input = open_input()?;
    let need = ((input.sample_rate as f32) * seconds).ceil() as usize;
    let mut samples = Vec::with_capacity(need);
    let deadline = Instant::now() + Duration::from_secs_f32(seconds + 3.0);

    while samples.len() < need {
        if Instant::now() > deadline {
            return Err(CaptureError::NoAudio);
        }
        match input.rx.recv_timeout(Duration::from_millis(200)) {
            Ok(chunk) => samples.extend(chunk),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return Err(CaptureError::NoAudio),
        }
    }
    samples.truncate(need);

    let peak = samples.iter().map(|sample| sample.abs()).fold(0.0, f32::max);
    let energy: f32 = samples.iter().map(|sample| sample * sample).sum();
    let rms = (energy / samples.len().max(1) as f32).sqrt();

    Ok(Probe {
        device_name: input.name,
        sample_rate: input.sample_rate,
        channels: input.channels,
        seconds: samples.len() as f32 / input.sample_rate as f32,
        rms,
        peak,
    })
}

/// Слушает микрофон, пока `running` истинен, и отдаёт события нарезки.
pub fn run(
    config: &Config,
    running: &AtomicBool,
    mut on_event: impl FnMut(Event),
) -> Result<(), CaptureError> {
    let input = open_input()?;
    on_event(Event::Ready(DeviceInfo {
        name: input.name,
        sample_rate: input.sample_rate,
        channels: input.channels,
    }));

    let mut segmenter = Segmenter::new(input.sample_rate, config);
    let mut was_speaking = false;
    let mut last_level = Instant::now();

    while running.load(Ordering::Relaxed) {
        let chunk = match input.rx.recv_timeout(Duration::from_millis(200)) {
            Ok(chunk) => chunk,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return Err(CaptureError::NoAudio),
        };

        let phrases = segmenter.push(&chunk);
        let speaking = segmenter.is_speaking();
        if speaking && !was_speaking {
            on_event(Event::SpeechStarted);
        }
        was_speaking = speaking;

        for phrase in phrases {
            on_event(Event::Phrase(phrase));
            // Пока фразу распознают и озвучивают, микрофон слышит колонки.
            // Эти кадры не распознаём: иначе ответ помощника уйдёт обратно в чат.
            while input.rx.try_recv().is_ok() {}
        }

        if !speaking && last_level.elapsed() >= Duration::from_secs(1) {
            on_event(Event::Listening {
                rms: segmenter.last_rms(),
            });
            last_level = Instant::now();
        }
    }

    if let Some(phrase) = segmenter.finish() {
        on_event(Event::Phrase(phrase));
    }
    Ok(())
}

struct Input {
    name: String,
    sample_rate: u32,
    channels: u16,
    rx: Receiver<Vec<f32>>,
    _stream: cpal::Stream,
}

fn open_input() -> Result<Input, CaptureError> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or(CaptureError::NoInputDevice)?;
    let name = device
        .name()
        .map_err(|err| CaptureError::Device(err.to_string()))?;
    let supported = device
        .default_input_config()
        .map_err(|err| CaptureError::Device(err.to_string()))?;
    let sample_format = supported.sample_format();
    let channels = supported.channels();
    let sample_rate = supported.sample_rate().0;
    let stream_config: cpal::StreamConfig = supported.into();
    let (tx, rx) = sync_channel(64);

    let stream = match sample_format {
        cpal::SampleFormat::F32 => build_stream::<f32>(&device, &stream_config, channels, tx),
        cpal::SampleFormat::I16 => build_stream::<i16>(&device, &stream_config, channels, tx),
        cpal::SampleFormat::U16 => build_stream::<u16>(&device, &stream_config, channels, tx),
        other => return Err(CaptureError::UnsupportedFormat(format!("{other:?}"))),
    }?;
    stream
        .play()
        .map_err(|err| CaptureError::Device(err.to_string()))?;

    Ok(Input {
        name,
        sample_rate,
        channels,
        rx,
        _stream: stream,
    })
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: u16,
    tx: SyncSender<Vec<f32>>,
) -> Result<cpal::Stream, CaptureError>
where
    T: SizedSample,
    f32: cpal::FromSample<T>,
{
    let channels = channels as usize;
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                let mono = mono_f32(data, channels);
                // Полный канал означает, что потребитель не успевает.
                // Кадр выбрасываем: для наброска это лучше, чем блокировать аудиопоток.
                let _ = tx.try_send(mono);
            },
            |err| eprintln!("микрофон: {err}"),
            None,
        )
        .map_err(|err| CaptureError::Device(err.to_string()))
}

fn mono_f32<T>(data: &[T], channels: usize) -> Vec<f32>
where
    T: Sample,
    f32: cpal::FromSample<T>,
{
    if channels <= 1 {
        return data.iter().map(|sample| sample.to_sample::<f32>()).collect();
    }
    data.chunks(channels)
        .map(|frame| {
            let sum: f32 = frame.iter().map(|sample| sample.to_sample::<f32>()).sum();
            sum / channels as f32
        })
        .collect()
}

struct Segmenter {
    input_rate: u32,
    threshold: f32,
    hangover_samples: usize,
    min_speech_samples: usize,
    preroll_capacity: usize,
    max_phrase_samples: usize,
    preroll: VecDeque<f32>,
    speech: Vec<f32>,
    preroll_copied: usize,
    silence_run: usize,
    in_speech: bool,
    last_rms: f32,
    pending: VecDeque<f32>,
    frame_len: usize,
}

impl Segmenter {
    fn new(input_rate: u32, config: &Config) -> Self {
        let frame_len = ms_to_samples(input_rate, 20).max(1);
        Self {
            input_rate,
            threshold: config.threshold,
            hangover_samples: ms_to_samples(input_rate, config.silence_ms),
            min_speech_samples: ms_to_samples(input_rate, config.min_speech_ms),
            preroll_capacity: ms_to_samples(input_rate, config.preroll_ms),
            max_phrase_samples: ms_to_samples(input_rate, config.max_phrase_ms),
            preroll: VecDeque::new(),
            speech: Vec::new(),
            preroll_copied: 0,
            silence_run: 0,
            in_speech: false,
            last_rms: 0.0,
            pending: VecDeque::new(),
            frame_len,
        }
    }

    fn is_speaking(&self) -> bool {
        self.in_speech
    }

    fn last_rms(&self) -> f32 {
        self.last_rms
    }

    fn push(&mut self, samples: &[f32]) -> Vec<Phrase> {
        self.pending.extend(samples.iter().copied());
        let mut phrases = Vec::new();
        while self.pending.len() >= self.frame_len {
            let frame: Vec<f32> = self.pending.drain(..self.frame_len).collect();
            if let Some(phrase) = self.consume_frame(&frame) {
                phrases.push(phrase);
            }
        }
        phrases
    }

    fn consume_frame(&mut self, frame: &[f32]) -> Option<Phrase> {
        let energy: f32 = frame.iter().map(|sample| sample * sample).sum();
        self.last_rms = (energy / frame.len().max(1) as f32).sqrt();

        if self.in_speech {
            self.speech.extend_from_slice(frame);
            if self.last_rms >= self.threshold {
                self.silence_run = 0;
            } else {
                self.silence_run += frame.len();
            }
            if self.silence_run >= self.hangover_samples
                || self.speech.len() >= self.max_phrase_samples
            {
                return self.finish();
            }
            return None;
        }

        self.preroll.extend(frame.iter().copied());
        while self.preroll.len() > self.preroll_capacity {
            self.preroll.pop_front();
        }
        if self.last_rms >= self.threshold {
            self.in_speech = true;
            self.preroll_copied = self.preroll.len();
            self.speech.extend(self.preroll.drain(..));
            self.silence_run = 0;
        }
        None
    }

    fn finish(&mut self) -> Option<Phrase> {
        let voiced = self
            .speech
            .len()
            .saturating_sub(self.preroll_copied)
            .saturating_sub(self.silence_run);
        let speech = std::mem::take(&mut self.speech);
        self.in_speech = false;
        self.silence_run = 0;
        self.preroll_copied = 0;
        if voiced < self.min_speech_samples {
            return None;
        }

        let samples = resample_linear(&speech, self.input_rate, OUTPUT_RATE);
        if samples.is_empty() {
            return None;
        }
        let peak = samples.iter().map(|sample| sample.abs()).fold(0.0, f32::max);
        let duration_ms = ((samples.len() as u64) * 1000 / u64::from(OUTPUT_RATE)) as u32;
        Some(Phrase {
            samples,
            sample_rate: OUTPUT_RATE,
            peak,
            duration_ms,
        })
    }
}

fn resample_linear(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if input.is_empty() || from == 0 || to == 0 {
        return Vec::new();
    }
    if from == to {
        return input.to_vec();
    }
    let out_len = ((input.len() as u64) * u64::from(to) / u64::from(from)) as usize;
    if out_len == 0 {
        return Vec::new();
    }
    let last = input.len() - 1;
    let scale = f64::from(from) / f64::from(to);
    let mut output = Vec::with_capacity(out_len);
    for index in 0..out_len {
        let position = index as f64 * scale;
        let left = position.floor() as usize;
        let fraction = (position - left as f64) as f32;
        let right = (left + 1).min(last);
        let start = input[left.min(last)];
        output.push(start + (input[right] - start) * fraction);
    }
    output
}

fn ms_to_samples(rate: u32, ms: u32) -> usize {
    ((u64::from(rate) * u64::from(ms)) / 1000) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            threshold: 0.02,
            silence_ms: 700,
            min_speech_ms: 300,
            preroll_ms: 200,
            max_phrase_ms: 30_000,
        }
    }

    fn tone(rate: u32, ms: u32, amplitude: f32) -> Vec<f32> {
        let count = ms_to_samples(rate, ms);
        (0..count)
            .map(|index| {
                let time = index as f32 / rate as f32;
                (2.0 * std::f32::consts::PI * 220.0 * time).sin() * amplitude
            })
            .collect()
    }

    fn silence(rate: u32, ms: u32) -> Vec<f32> {
        vec![0.0; ms_to_samples(rate, ms)]
    }

    #[test]
    fn one_phrase_after_silence() {
        let mut segmenter = Segmenter::new(16_000, &config());
        let mut audio = silence(16_000, 500);
        audio.extend(tone(16_000, 500, 0.2));
        audio.extend(silence(16_000, 1_000));
        let phrases = segmenter.push(&audio);
        assert_eq!(phrases.len(), 1);
        assert_eq!(phrases[0].sample_rate, OUTPUT_RATE);
        assert!(phrases[0].peak > 0.1);
        assert!(phrases[0].duration_ms > 800);
        assert!(phrases[0].duration_ms < 2_000);
    }

    #[test]
    fn short_blip_is_dropped() {
        let mut segmenter = Segmenter::new(16_000, &config());
        let mut audio = silence(16_000, 500);
        audio.extend(tone(16_000, 100, 0.2));
        audio.extend(silence(16_000, 1_000));
        assert!(segmenter.push(&audio).is_empty());
    }

    #[test]
    fn two_phrases_stay_separate() {
        let mut segmenter = Segmenter::new(16_000, &config());
        let mut audio = silence(16_000, 300);
        audio.extend(tone(16_000, 400, 0.2));
        audio.extend(silence(16_000, 1_000));
        audio.extend(tone(16_000, 400, 0.2));
        audio.extend(silence(16_000, 1_000));
        assert_eq!(segmenter.push(&audio).len(), 2);
    }

    #[test]
    fn resample_keeps_length_ratio() {
        let input = tone(48_000, 1_000, 0.2);
        let output = resample_linear(&input, 48_000, 16_000);
        assert!((output.len() as i32 - 16_000).abs() < 5);
    }
}
