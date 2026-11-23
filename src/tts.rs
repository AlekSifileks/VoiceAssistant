//! Озвучка ответа. Короткие подтверждения команд говорит Милена.
//! Реплики чата считает локальный Qwen3-TTS. Контракт — `docs/modules/tts.md`.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::Sample;

pub const DEFAULT_VOICE: &str = "models/annie-voice.wav";
pub const DEFAULT_MODEL: &str = "mlx-community/Qwen3-TTS-12Hz-1.7B-Base-8bit";
pub const DEFAULT_DESIGN_MODEL: &str = "mlx-community/Qwen3-TTS-12Hz-1.7B-VoiceDesign-8bit";
pub const DEFAULT_ACK_VOICE: &str = "Milena";

#[derive(Debug, Clone)]
pub struct Config {
    pub provider: String,
    pub model: String,
    pub design_model: String,
    pub voice: PathBuf,
    pub ack_voice: String,
    pub python: PathBuf,
    pub script: PathBuf,
}

impl Config {
    pub fn from_env() -> Self {
        let provider = std::env::var("TTS_PROVIDER").unwrap_or_else(|_| "qwen".into());
        let provider = if provider.trim().is_empty() {
            "qwen".into()
        } else {
            provider.trim().to_ascii_lowercase()
        };
        Self {
            provider,
            model: env_or("TTS_MODEL", DEFAULT_MODEL),
            design_model: env_or("TTS_DESIGN_MODEL", DEFAULT_DESIGN_MODEL),
            voice: PathBuf::from(env_or("TTS_VOICE", DEFAULT_VOICE)),
            ack_voice: env_or("TTS_ACK_VOICE", DEFAULT_ACK_VOICE),
            python: PathBuf::from(env_or("TTS_PYTHON", "tts/.venv/bin/python")),
            script: PathBuf::from(env_or("TTS_SCRIPT", "tts/qwen_server.py")),
        }
    }
}

pub struct Speaker {
    config: Config,
    session: Option<Session>,
    reply_error: Option<String>,
    utterance: u64,
}

struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
}

impl Speaker {
    /// Поднимает голос чата. Подтверждения команд работают и без него.
    pub fn start(config: Config) -> Self {
        if config.provider != "qwen" {
            return Self {
                reply_error: Some("эта озвучка пока не подключена".into()),
                config,
                session: None,
                utterance: 0,
            };
        }
        match Session::launch(&config) {
            Ok(session) => Self {
                config,
                session: Some(session),
                reply_error: None,
                utterance: 0,
            },
            Err(err) => Self {
                config,
                session: None,
                reply_error: Some(err),
                utterance: 0,
            },
        }
    }

    pub fn reply_ready(&self) -> bool {
        self.session.is_some()
    }

    pub fn acknowledge(&mut self, success: bool) {
        let phrase = ack_phrase(success);
        if let Err(err) = speak_system(&self.config.ack_voice, phrase) {
            eprintln!("озвучка: {err}");
        }
    }

    pub fn say(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let Some(session) = self.session.as_mut() else {
            if let Some(err) = &self.reply_error {
                eprintln!("озвучка: {err}");
            }
            return;
        };
        self.utterance += 1;
        let path = std::env::temp_dir().join(format!(
            "voice-assistant-reply-{}-{}.wav",
            std::process::id(),
            self.utterance
        ));
        if let Err(err) = session.synthesize(text, &path).and_then(|_| play_wav(&path)) {
            eprintln!("озвучка: {err}");
        }
        let _ = std::fs::remove_file(&path);
    }
}

impl Drop for Speaker {
    fn drop(&mut self) {
        if let Some(session) = self.session.as_mut() {
            let _ = writeln!(session.stdin, r#"{{"cmd":"stop"}}"#);
            let _ = session.child.kill();
        }
    }
}

impl Session {
    fn launch(config: &Config) -> Result<Self, String> {
        if !config.python.is_file() {
            return Err(format!(
                "нет интерпретатора {}. В каталоге проекта: python3 -m venv tts/.venv && tts/.venv/bin/pip install -r tts/requirements.txt",
                config.python.display()
            ));
        }
        if !config.script.is_file() {
            return Err(format!("нет расчёта голоса {}", config.script.display()));
        }
        let mut child = Command::new(&config.python)
            .arg(&config.script)
            .env("PYTHONUNBUFFERED", "1")
            .env("TTS_MODEL", &config.model)
            .env("TTS_DESIGN_MODEL", &config.design_model)
            .env("TTS_VOICE", &config.voice)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| format!("не запустил голос: {err}"))?;
        let stdin = child.stdin.take().ok_or("у голоса нет входа")?;
        let stdout = child.stdout.take().ok_or("у голоса нет выхода")?;
        let stderr = child.stderr.take().ok_or("у голоса нет журнала")?;
        thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(line) if !line.trim().is_empty() => eprintln!("голос: {line}"),
                    _ => {}
                }
            }
        });
        let mut stdout = BufReader::new(stdout);
        let mut ready = String::new();
        stdout
            .read_line(&mut ready)
            .map_err(|err| format!("голос не ответил: {err}"))?;
        if ready.trim().is_empty() {
            return Err("голос завершился до загрузки".into());
        }
        let value: serde_json::Value = serde_json::from_str(ready.trim())
            .map_err(|err| format!("голос прислал не json: {err}: {ready}"))?;
        if value.get("ready").and_then(|item| item.as_bool()) != Some(true) {
            let message = value
                .get("error")
                .and_then(|item| item.as_str())
                .unwrap_or("голос не загрузился");
            return Err(message.to_string());
        }
        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }

    fn synthesize(&mut self, text: &str, path: &Path) -> Result<(), String> {
        let request = serde_json::json!({
            "text": text,
            "wav": path,
        });
        writeln!(self.stdin, "{request}").map_err(|err| format!("не передал текст голосу: {err}"))?;
        self.stdin
            .flush()
            .map_err(|err| format!("не передал текст голосу: {err}"))?;
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .map_err(|err| format!("голос не ответил: {err}"))?;
        if line.trim().is_empty() {
            return Err("голос закрыл ответ".into());
        }
        let value: serde_json::Value = serde_json::from_str(line.trim())
            .map_err(|err| format!("голос прислал не json: {err}"))?;
        if value.get("ok").and_then(|item| item.as_bool()) == Some(true) {
            return Ok(());
        }
        Err(value
            .get("error")
            .and_then(|item| item.as_str())
            .unwrap_or("голос не собрал фразу")
            .to_string())
    }
}

pub fn ack_phrase(success: bool) -> &'static str {
    if success {
        "Готово"
    } else {
        "Не вышло"
    }
}

fn speak_system(voice: &str, text: &str) -> Result<(), String> {
    let path = std::env::temp_dir().join(format!(
        "voice-assistant-ack-{}-{}.wav",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let status = Command::new("say")
        .args([
            "-v",
            voice,
            "-o",
            &path.to_string_lossy(),
            "--file-format=WAVE",
            "--data-format=LEI16@22050",
            text,
        ])
        .status()
        .map_err(|err| format!("не удалось вызвать say: {err}"))?;
    if !status.success() {
        let _ = std::fs::remove_file(&path);
        return Err(format!("системный голос завершился с {status}"));
    }
    let played = play_wav(&path);
    let _ = std::fs::remove_file(&path);
    played
}

fn play_wav(path: &Path) -> Result<(), String> {
    let (samples, rate) = read_wav_mono(path)?;
    if samples.is_empty() {
        return Err("пустой звук".into());
    }
    play_samples(&samples, rate)
}

fn read_wav_mono(path: &Path) -> Result<(Vec<f32>, u32), String> {
    let mut reader = hound::WavReader::open(path).map_err(|err| format!("{err}"))?;
    let spec = reader.spec();
    let channels = spec.channels.max(1) as usize;
    let raw = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| err.to_string())?,
        hound::SampleFormat::Int => {
            let scale = (1i64 << (spec.bits_per_sample.saturating_sub(1))) as f32;
            reader
                .samples::<i32>()
                .map(|sample| sample.map(|value| value as f32 / scale))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|err| err.to_string())?
        }
    };
    let mono = if channels == 1 {
        raw
    } else {
        raw.chunks(channels)
            .map(|frame| frame.iter().sum::<f32>() / channels as f32)
            .collect()
    };
    Ok((mono, spec.sample_rate))
}

fn play_samples(samples: &[f32], rate: u32) -> Result<(), String> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or("нет устройства вывода")?;
    let supported = device
        .default_output_config()
        .map_err(|err| err.to_string())?;
    let channels = supported.channels() as usize;
    let sample_format = supported.sample_format();
    let device_rate = supported.sample_rate().0;
    let config: cpal::StreamConfig = supported.into();
    let mono = resample(samples, rate, device_rate);
    let done = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&done);
    let stream = match sample_format {
        cpal::SampleFormat::F32 => output_stream::<f32>(&device, &config, channels, mono, flag)?,
        cpal::SampleFormat::I16 => output_stream::<i16>(&device, &config, channels, mono, flag)?,
        cpal::SampleFormat::U16 => output_stream::<u16>(&device, &config, channels, mono, flag)?,
        other => return Err(format!("формат вывода не поддержан: {other:?}")),
    };
    stream.play().map_err(|err| err.to_string())?;
    while !done.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(20));
    }
    thread::sleep(Duration::from_millis(200));
    drop(stream);
    Ok(())
}

fn output_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    mono: Vec<f32>,
    done: Arc<AtomicBool>,
) -> Result<cpal::Stream, String>
where
    T: Sample + cpal::SizedSample + cpal::FromSample<f32>,
{
    let mut cursor = 0usize;
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                for frame in data.chunks_mut(channels.max(1)) {
                    let sample = mono.get(cursor).copied().unwrap_or(0.0);
                    if cursor < mono.len() {
                        cursor += 1;
                    }
                    for slot in frame.iter_mut() {
                        *slot = T::from_sample(sample);
                    }
                }
                if cursor >= mono.len() {
                    done.store(true, Ordering::Relaxed);
                }
            },
            |err| eprintln!("звук: {err}"),
            None,
        )
        .map_err(|err| err.to_string())
}

fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == 0 || to == 0 || from == to || input.is_empty() {
        return input.to_vec();
    }
    let out_len = ((input.len() as u64 * u64::from(to)) / u64::from(from)).max(1) as usize;
    let mut out = Vec::with_capacity(out_len);
    for index in 0..out_len {
        let position = index as f64 * f64::from(from) / f64::from(to);
        let left = position.floor() as usize;
        let fraction = (position - left as f64) as f32;
        let a = input.get(left).copied().unwrap_or(0.0);
        let b = input.get(left + 1).copied().unwrap_or(a);
        out.push(a + (b - a) * fraction);
    }
    out
}

fn env_or(key: &str, default: &str) -> String {
    match std::env::var(key) {
        Ok(value) if !value.trim().is_empty() => value,
        _ => default.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_ack_is_one_fixed_word() {
        assert_eq!(ack_phrase(true), "Готово");
        assert_eq!(ack_phrase(false), "Не вышло");
    }

    #[test]
    fn resample_keeps_length_when_rates_match() {
        let input = vec![0.0, 0.5, -0.5];
        assert_eq!(resample(&input, 16_000, 16_000), input);
    }

    #[test]
    fn milena_writes_a_wav_without_playing_it() {
        let path = std::env::temp_dir().join(format!(
            "voice-assistant-say-{}-{}.wav",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let status = Command::new("say")
            .args([
                "-v",
                "Milena",
                "-o",
                &path.to_string_lossy(),
                "--file-format=WAVE",
                "--data-format=LEI16@22050",
                "Готово",
            ])
            .status()
            .unwrap();
        assert!(status.success());
        let (samples, rate) = read_wav_mono(&path).unwrap();
        assert_eq!(rate, 22_050);
        assert!(samples.len() > 1000);
        let _ = std::fs::remove_file(&path);
    }
}
