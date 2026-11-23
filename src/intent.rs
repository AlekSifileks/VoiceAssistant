//! Локальное сравнение фразы с известными командами.
//!
//! Модель загружается один раз и живёт, пока запущен помощник.
//! Контракт описан в `docs/modules/intent.md`.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::thread;

pub const DEFAULT_MODEL: &str = "cointegrated/rubert-tiny2";

#[derive(Debug, Clone)]
pub struct Config {
    pub model: String,
    pub python: PathBuf,
    pub script: PathBuf,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            model: env_or("INTENT_MODEL", DEFAULT_MODEL),
            python: PathBuf::from(env_or("INTENT_PYTHON", "nlu/.venv/bin/python")),
            script: PathBuf::from(env_or("INTENT_SCRIPT", "nlu/intent_server.py")),
        }
    }
}

pub struct Model {
    config: Config,
    session: Option<Session>,
    start_error: Option<String>,
}

struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
}

pub struct ScoredLabel {
    pub label: String,
    pub score: f32,
}

pub struct Guess {
    pub label: String,
    pub score: f32,
    pub scores: Vec<ScoredLabel>,
}

impl Model {
    /// Поднимает модель. Если её нет, команды продолжают работать дословно.
    pub fn start(config: Config) -> Self {
        match Session::launch(&config) {
            Ok(session) => Self {
                config,
                session: Some(session),
                start_error: None,
            },
            Err(err) => Self {
                config,
                session: None,
                start_error: Some(err),
            },
        }
    }

    pub fn ready(&self) -> bool {
        self.session.is_some()
    }

    pub fn model_label(&self) -> &str {
        &self.config.model
    }

    pub fn start_error(&self) -> Option<&str> {
        self.start_error.as_deref()
    }

    /// Ближайшая подпись из `options`. Пустой список и неготовая модель дают ошибку.
    pub fn guess(&mut self, text: &str, options: &[String]) -> Result<Guess, String> {
        let Some(session) = self.session.as_mut() else {
            return Err(self
                .start_error
                .clone()
                .unwrap_or_else(|| "понимание не запущено".into()));
        };
        session.guess(text, options)
    }
}

impl Drop for Model {
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
                "нет интерпретатора {}. В каталоге проекта: python3 -m venv nlu/.venv && nlu/.venv/bin/pip install -r nlu/requirements.txt",
                config.python.display()
            ));
        }
        if !config.script.is_file() {
            return Err(format!("нет расчёта понимания {}", config.script.display()));
        }
        let mut child = Command::new(&config.python)
            .arg(&config.script)
            .env("PYTHONUNBUFFERED", "1")
            .env("TRANSFORMERS_VERBOSITY", "error")
            .env("HF_HUB_DISABLE_PROGRESS_BARS", "1")
            .env("TOKENIZERS_PARALLELISM", "false")
            .env("INTENT_MODEL", &config.model)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| format!("не запустил понимание: {err}"))?;
        let stdin = child.stdin.take().ok_or("у понимания нет входа")?;
        let stdout = child.stdout.take().ok_or("у понимания нет выхода")?;
        let stderr = child.stderr.take().ok_or("у понимания нет журнала")?;
        thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(line) if !line.trim().is_empty() => eprintln!("понимание: {line}"),
                    _ => {}
                }
            }
        });
        let mut stdout = BufReader::new(stdout);
        let mut ready = String::new();
        stdout
            .read_line(&mut ready)
            .map_err(|err| format!("понимание не ответило: {err}"))?;
        if ready.trim().is_empty() {
            return Err("понимание завершилось до загрузки".into());
        }
        let value: serde_json::Value = serde_json::from_str(ready.trim())
            .map_err(|err| format!("понимание прислало не json: {err}: {ready}"))?;
        if value.get("ready").and_then(|item| item.as_bool()) != Some(true) {
            let message = value
                .get("error")
                .and_then(|item| item.as_str())
                .unwrap_or("модель не загрузилась");
            return Err(message.to_string());
        }
        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }

    fn guess(&mut self, text: &str, options: &[String]) -> Result<Guess, String> {
        let request = serde_json::json!({
            "text": text,
            "options": options,
        });
        writeln!(self.stdin, "{request}").map_err(|err| format!("не передал фразу: {err}"))?;
        self.stdin
            .flush()
            .map_err(|err| format!("не передал фразу: {err}"))?;
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .map_err(|err| format!("понимание не ответило: {err}"))?;
        if line.trim().is_empty() {
            return Err("понимание закрыло ответ".into());
        }
        let value: serde_json::Value = serde_json::from_str(line.trim())
            .map_err(|err| format!("понимание прислало не json: {err}"))?;
        if value.get("ok").and_then(|item| item.as_bool()) != Some(true) {
            return Err(value
                .get("error")
                .and_then(|item| item.as_str())
                .unwrap_or("понимание не выбрало команду")
                .to_string());
        }
        let label = value
            .get("label")
            .and_then(|item| item.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let score = value
            .get("score")
            .and_then(|item| item.as_f64())
            .unwrap_or(0.0) as f32;
        if label.is_empty() {
            return Err("понимание не назвало команду".into());
        }
        let scores = value
            .get("scores")
            .and_then(|item| item.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let label = item.get("label")?.as_str()?.trim().to_string();
                        let score = item.get("score")?.as_f64()? as f32;
                        if label.is_empty() {
                            None
                        } else {
                            Some(ScoredLabel { label, score })
                        }
                    })
                    .collect()
            })
            .filter(|items: &Vec<_>| !items.is_empty())
            .unwrap_or_else(|| vec![ScoredLabel { label: label.clone(), score }]);
        Ok(Guess {
            label,
            score,
            scores,
        })
    }
}

fn env_or(key: &str, fallback: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}
