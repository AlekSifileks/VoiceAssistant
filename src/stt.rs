//! Локальное распознавание одной фразы.
//!
//! Модель загружается один раз. Модуль не открывает микрофон и не ходит в сеть.
//! Контракт описан в `docs/modules/stt.md`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

pub const DEFAULT_MODEL: &str = "models/ggml-large-v3-turbo.bin";

pub struct Engine {
    state: WhisperState,
    language: Option<String>,
}

pub struct Transcript {
    pub text: String,
    pub elapsed: Duration,
}

#[derive(Debug)]
pub enum SttError {
    MissingModel(PathBuf),
    Load(String),
    Transcribe(String),
}

impl std::fmt::Display for SttError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingModel(path) => write!(
                f,
                "нет файла модели {}. Положите туда ggml-large-v3-turbo.bin",
                path.display()
            ),
            Self::Load(message) => write!(f, "не удалось загрузить модель: {message}"),
            Self::Transcribe(message) => write!(f, "распознавание не выполнилось: {message}"),
        }
    }
}

impl std::error::Error for SttError {}

impl Engine {
    /// `language` — код Whisper (`ru`, `en`) или `None` для автоопределения.
    pub fn load(path: &Path, language: Option<&str>) -> Result<Self, SttError> {
        if !path.is_file() {
            return Err(SttError::MissingModel(path.to_path_buf()));
        }
        // Без фичи log_backend хуки глушат stderr whisper.cpp. В терминале остаётся наш текст.
        whisper_rs::install_logging_hooks();
        let context = WhisperContext::new_with_params(path, WhisperContextParameters::default())
            .map_err(|err| SttError::Load(err.to_string()))?;
        let state = context
            .create_state()
            .map_err(|err| SttError::Load(err.to_string()))?;
        let language = language
            .map(str::trim)
            .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("auto"))
            .map(str::to_string);
        Ok(Self { state, language })
    }

    pub fn language_label(&self) -> &str {
        self.language.as_deref().unwrap_or("авто")
    }

    /// Пустой звук и известные галлюцинации тишины возвращают `None`.
    pub fn transcribe(&mut self, samples: &[f32]) -> Result<Option<Transcript>, SttError> {
        if samples.is_empty() {
            return Ok(None);
        }

        let language = self.language.clone();
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_n_threads(decoder_threads());
        params.set_translate(false);
        params.set_no_context(true);
        params.set_single_segment(true);
        params.set_suppress_blank(true);
        params.set_suppress_nst(true);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_temperature(0.0);
        match language.as_deref() {
            Some(language) => {
                params.set_language(Some(language));
                params.set_detect_language(false);
            }
            None => {
                params.set_language(Some("auto"));
                params.set_detect_language(true);
            }
        }

        let started = std::time::Instant::now();
        self.state
            .full(params, samples)
            .map_err(|err| SttError::Transcribe(err.to_string()))?;
        let elapsed = started.elapsed();

        let mut text = String::new();
        for segment in self.state.as_iter() {
            let piece = segment
                .to_str_lossy()
                .map_err(|err| SttError::Transcribe(err.to_string()))?;
            if !text.is_empty() && !piece.starts_with(' ') {
                text.push(' ');
            }
            text.push_str(piece.trim_end());
        }
        let text = text.trim().to_string();
        if text.is_empty() || is_silence_hallucination(&text) {
            return Ok(None);
        }
        Ok(Some(Transcript { text, elapsed }))
    }
}

fn decoder_threads() -> i32 {
    let count = std::thread::available_parallelism()
        .map(|threads| threads.get() as i32)
        .unwrap_or(4);
    count.clamp(1, 8)
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
    )         || normalized.starts_with("субтитры ")
        || normalized.starts_with("subtitles by ")
}

#[cfg(test)]
mod tests {
    use super::is_silence_hallucination;

    #[test]
    fn drops_known_silence_phrases() {
        assert!(is_silence_hallucination("Субтитры создавал DimaTorzok"));
        assert!(is_silence_hallucination("Thanks for watching!"));
        assert!(!is_silence_hallucination("открой курсор"));
    }
}
