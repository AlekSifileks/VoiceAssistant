//! Локальное распознавание одной фразы моделью GigaAM v3.
//!
//! Расчёт идёт в отдельном процессе на MLX. Модуль не открывает микрофон.
//! Контракт описан в `docs/modules/stt.md`.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::thread;
use std::time::Duration;

pub const DEFAULT_MODEL: &str = "rnnt";

pub struct Config {
    pub model: String,
    pub python: PathBuf,
    pub script: PathBuf,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            model: env_or("STT_MODEL", DEFAULT_MODEL),
            python: PathBuf::from(env_or("STT_PYTHON", "stt/.venv/bin/python")),
            script: PathBuf::from(env_or("STT_SCRIPT", "stt/gigaam_server.py")),
        }
    }
}

pub struct Engine {
    session: Session,
    model: String,
    utterance: u64,
}

pub struct Transcript {
    pub text: String,
    pub elapsed: Duration,
}

struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
}

#[derive(Debug)]
pub enum SttError {
    Language(String),
    Start(String),
    Transcribe(String),
}

impl std::fmt::Display for SttError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Language(code) => write!(
                f,
                "GigaAM v3 слушает русский, --language {code} не поддержан"
            ),
            Self::Start(message) => write!(f, "{message}"),
            Self::Transcribe(message) => write!(f, "распознавание не выполнилось: {message}"),
        }
    }
}

impl std::error::Error for SttError {}

impl Engine {
    /// `language` — `ru`, пусто или `auto`. Другой код эта модель не переключает.
    pub fn load(config: Config, language: Option<&str>) -> Result<Self, SttError> {
        if let Some(code) = language.map(str::trim).filter(|code| !code.is_empty()) {
            if !code.eq_ignore_ascii_case("ru") && !code.eq_ignore_ascii_case("auto") {
                return Err(SttError::Language(code.to_string()));
            }
        }
        let session = Session::launch(&config)?;
        Ok(Self {
            session,
            model: config.model,
            utterance: 0,
        })
    }

    pub fn model_label(&self) -> String {
        format!("GigaAM v3 {}", self.model)
    }

    /// Пустой звук и текст без слов возвращают `None`.
    pub fn transcribe(&mut self, samples: &[f32]) -> Result<Option<Transcript>, SttError> {
        if samples.is_empty() {
            return Ok(None);
        }
        self.utterance += 1;
        let path = std::env::temp_dir().join(format!(
            "voice-assistant-phrase-{}-{}.wav",
            std::process::id(),
            self.utterance
        ));
        write_wav(&path, samples).map_err(SttError::Transcribe)?;
        let started = std::time::Instant::now();
        let text = self.session.transcribe(&path);
        let elapsed = started.elapsed();
        let _ = std::fs::remove_file(&path);
        let text = text.map_err(SttError::Transcribe)?;
        if !has_words(&text) || is_silence_hallucination(&text) {
            return Ok(None);
        }
        Ok(Some(Transcript { text, elapsed }))
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = writeln!(self.session.stdin, r#"{{"cmd":"stop"}}"#);
        let _ = self.session.child.kill();
    }
}

impl Session {
    fn launch(config: &Config) -> Result<Self, SttError> {
        if !config.python.is_file() {
            return Err(SttError::Start(format!(
                "нет интерпретатора {}. В каталоге проекта: python3 -m venv stt/.venv && stt/.venv/bin/pip install -r stt/requirements.txt",
                config.python.display()
            )));
        }
        if !config.script.is_file() {
            return Err(SttError::Start(format!(
                "нет расчёта речи {}",
                config.script.display()
            )));
        }
        let mut child = Command::new(&config.python)
            .arg(&config.script)
            .env("PYTHONUNBUFFERED", "1")
            .env("STT_MODEL", &config.model)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| SttError::Start(format!("не запустил распознавание: {err}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| SttError::Start("у распознавания нет входа".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| SttError::Start("у распознавания нет выхода".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| SttError::Start("у распознавания нет журнала".into()))?;
        thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(line) if !line.trim().is_empty() => eprintln!("речь: {line}"),
                    _ => {}
                }
            }
        });
        let mut stdout = BufReader::new(stdout);
        let mut ready = String::new();
        stdout
            .read_line(&mut ready)
            .map_err(|err| SttError::Start(format!("распознавание не ответило: {err}")))?;
        if ready.trim().is_empty() {
            return Err(SttError::Start(
                "распознавание завершилось до загрузки".into(),
            ));
        }
        let value: serde_json::Value = serde_json::from_str(ready.trim()).map_err(|err| {
            SttError::Start(format!("распознавание прислало не json: {err}: {ready}"))
        })?;
        if value.get("ready").and_then(|item| item.as_bool()) != Some(true) {
            let message = value
                .get("error")
                .and_then(|item| item.as_str())
                .unwrap_or("модель не загрузилась");
            return Err(SttError::Start(message.to_string()));
        }
        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }

    fn transcribe(&mut self, path: &Path) -> Result<String, String> {
        let request = serde_json::json!({ "wav": path });
        writeln!(self.stdin, "{request}")
            .map_err(|err| format!("не передал фразу: {err}"))?;
        self.stdin
            .flush()
            .map_err(|err| format!("не передал фразу: {err}"))?;
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .map_err(|err| format!("распознавание не ответило: {err}"))?;
        if line.trim().is_empty() {
            return Err("распознавание закрыло ответ".into());
        }
        let value: serde_json::Value = serde_json::from_str(line.trim())
            .map_err(|err| format!("распознавание прислало не json: {err}"))?;
        if value.get("ok").and_then(|item| item.as_bool()) == Some(true) {
            return Ok(value
                .get("text")
                .and_then(|item| item.as_str())
                .unwrap_or("")
                .trim()
                .to_string());
        }
        Err(value
            .get("error")
            .and_then(|item| item.as_str())
            .unwrap_or("распознавание не вернуло текст")
            .to_string())
    }
}

fn write_wav(path: &Path, samples: &[f32]) -> Result<(), String> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .map_err(|err| format!("не записал фразу: {err}"))?;
    for sample in samples {
        let clamped = sample.clamp(-1.0, 1.0);
        let value = (clamped * 32767.0).round() as i16;
        writer
            .write_sample(value)
            .map_err(|err| format!("не записал фразу: {err}"))?;
    }
    writer
        .finalize()
        .map_err(|err| format!("не записал фразу: {err}"))
}

fn env_or(key: &str, fallback: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

fn has_words(text: &str) -> bool {
    text.chars().any(|ch| ch.is_alphanumeric())
}

fn is_silence_hallucination(text: &str) -> bool {
    let normalized: String = text
        .chars()
        .flat_map(|ch| ch.to_lowercase())
        .filter(|ch| ch.is_alphanumeric() || ch.is_whitespace())
        .collect();
    let normalized = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    matches!(
        normalized.as_str(),
        "спасибо за просмотр"
            | "thanks for watching"
            | "продолжение следует"
            | "субтитры"
            | "субтитры создавал dimatorzok"
            | "subtitles by"
    ) || normalized.starts_with("субтитры ")
        || normalized.starts_with("subtitles by ")
}

#[cfg(test)]
mod tests {
    use super::{has_words, is_silence_hallucination, write_wav};

    #[test]
    fn drops_known_silence_phrases() {
        assert!(is_silence_hallucination("Субтитры создавал DimaTorzok"));
        assert!(is_silence_hallucination("Thanks for watching!"));
        assert!(!is_silence_hallucination("открой курсор"));
    }

    #[test]
    fn phrase_file_is_plain_pcm() {
        let path = std::env::temp_dir().join(format!(
            "voice-assistant-wav-{}-{}.wav",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_wav(&path, &[0.0, 0.5, -0.5]).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let format = u16::from_le_bytes([bytes[20], bytes[21]]);
        assert_eq!(format, 1, "временный wav должен быть целым PCM, не дробным");
    }

    #[test]
    fn punctuation_alone_is_not_speech() {
        assert!(!has_words("..."));
        assert!(has_words("Лика, пуск курсор."));
    }
}
