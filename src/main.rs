//! Приём фразы, распознавание и команды по обращению.
//!
//! Микрофон режет речь, Whisper печатает текст. «Эни пуск …» открывает приложение.
//! Сеть помощника не открывается. Модули: `docs/modules/`.

mod capture;
mod commands;
mod stt;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use capture::{CaptureError, Config, Event};
use commands::{Outcome, Settings};
use stt::{Engine, SttError};

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), AppError> {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };

    if args.probe {
        let report = capture::probe(1.0)?;
        println!(
            "микрофон: {} @ {} Гц, {} кан.",
            report.device_name, report.sample_rate, report.channels
        );
        println!(
            "проба {:.2} с: уровень {:.4}, пик {:.4}",
            report.seconds, report.rms, report.peak
        );
        return Ok(());
    }

    let commands = Settings::from_env();
    if commands.enabled {
        println!(
            "команды включены, обращение «{}», закладки {}",
            commands.address(),
            commands.database.display()
        );
    } else {
        println!("команды выключены");
    }

    let started = Instant::now();
    let mut engine = Engine::load(&args.model, args.language.as_deref())?;
    println!(
        "модель {} загружена за {:.1} с, язык: {}",
        args.model.display(),
        started.elapsed().as_secs_f32(),
        engine.language_label()
    );

    if let Some(path) = args.file {
        let samples = read_wav(&path)?;
        if let Some(text) = recognize(&mut engine, &samples)? {
            report_command(commands::handle(&text, &commands));
        }
        return Ok(());
    }

    std::fs::create_dir_all(&args.out).map_err(|err| AppError::Message(err.to_string()))?;

    let running = Arc::new(AtomicBool::new(true));
    let flag = Arc::clone(&running);
    if ctrlc::set_handler(move || flag.store(false, Ordering::Relaxed)).is_err() {
        eprintln!("не удалось перехватить Ctrl+C, остановка только закрытием процесса");
    }

    let config = Config {
        threshold: args.threshold,
        silence_ms: args.silence_ms,
        ..Config::default()
    };
    println!(
        "слушаю. порог {:.3}, тишина {} мс. Ctrl+C останавливает. wav: {}",
        config.threshold,
        config.silence_ms,
        args.out.display()
    );

    let mut index = 0u32;
    let out_dir = args.out;
    capture::run(&config, &running, |event| match event {
        Event::Ready(info) => println!(
            "микрофон: {} @ {} Гц, {} кан.",
            info.name, info.sample_rate, info.channels
        ),
        Event::Listening { rms } => println!("тишина, уровень {rms:.4}"),
        Event::SpeechStarted => println!("слышу речь…"),
        Event::Phrase(phrase) => {
            index += 1;
            let path = out_dir.join(format!("phrase-{index:03}.wav"));
            match write_wav(&path, &phrase) {
                Ok(()) => println!(
                    "фраза {index}: {:.2} с, пик {:.3}, {} сэмплов @ {} Гц, {}",
                    phrase.duration_ms as f32 / 1000.0,
                    phrase.peak,
                    phrase.samples.len(),
                    phrase.sample_rate,
                    path.display()
                ),
                Err(err) => eprintln!("не записал {}: {err}", path.display()),
            }
            match recognize(&mut engine, &phrase.samples) {
                Ok(Some(text)) => report_command(commands::handle(&text, &commands)),
                Ok(None) => {}
                Err(err) => eprintln!("{err}"),
            }
        }
    })?;

    println!("остановлено");
    Ok(())
}

struct Args {
    probe: bool,
    out: PathBuf,
    threshold: f32,
    silence_ms: u32,
    model: PathBuf,
    language: Option<String>,
    file: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        probe: false,
        out: PathBuf::from("captures"),
        threshold: Config::default().threshold,
        silence_ms: Config::default().silence_ms,
        model: PathBuf::from(stt::DEFAULT_MODEL),
        language: None,
        file: None,
    };
    let mut tokens = std::env::args().skip(1);
    while let Some(token) = tokens.next() {
        match token.as_str() {
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            "--probe" => args.probe = true,
            "--out" => {
                args.out = PathBuf::from(next_value(&mut tokens, "--out")?);
            }
            "--threshold" => {
                let raw = next_value(&mut tokens, "--threshold")?;
                args.threshold = raw
                    .parse()
                    .map_err(|_| format!("--threshold: не число: {raw}"))?;
                if args.threshold <= 0.0 {
                    return Err("--threshold должен быть больше нуля".into());
                }
            }
            "--silence-ms" => {
                let raw = next_value(&mut tokens, "--silence-ms")?;
                args.silence_ms = raw
                    .parse()
                    .map_err(|_| format!("--silence-ms: не целое: {raw}"))?;
                if args.silence_ms == 0 {
                    return Err("--silence-ms должен быть больше нуля".into());
                }
            }
            "--model" => {
                args.model = PathBuf::from(next_value(&mut tokens, "--model")?);
            }
            "--language" => {
                args.language = Some(next_value(&mut tokens, "--language")?);
            }
            "--file" => {
                args.file = Some(PathBuf::from(next_value(&mut tokens, "--file")?));
            }
            other => return Err(format!("неизвестный аргумент: {other}\n{}", usage())),
        }
    }
    Ok(args)
}

fn next_value(tokens: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    tokens
        .next()
        .ok_or_else(|| format!("{flag} требует значение"))
}

fn print_usage() {
    println!("{}", usage());
}

fn usage() -> &'static str {
    "\
voice-assistant — слушает микрофон, пишет wav и печатает текст. Никуда не отправляет.

  cargo run --
  cargo run -- --probe
  cargo run -- --file captures/phrase-001.wav
  cargo run -- --language ru

  --probe           секунда с микрофона и уровень, без модели и без записи
  --file PATH       распознать готовый wav 16 кГц моно и выйти
  --model PATH      файл ggml (по умолчанию models/ggml-large-v3-turbo.bin)
  --language CODE   язык речи, например ru или en; по умолчанию авто
  --out DIR         куда писать phrase-NNN.wav (по умолчанию captures)
  --threshold N     уровень, с которого кадр считается речью (по умолчанию 0.02)
  --silence-ms N    пауза, которая закрывает фразу (по умолчанию 700)"
}

fn recognize(engine: &mut Engine, samples: &[f32]) -> Result<Option<String>, SttError> {
    match engine.transcribe(samples)? {
        Some(transcript) => {
            println!(
                "текст ({} мс): {}",
                transcript.elapsed.as_millis(),
                transcript.text
            );
            Ok(Some(transcript.text))
        }
        None => {
            println!("текста нет");
            Ok(None)
        }
    }
}

fn report_command(outcome: Outcome) {
    match outcome {
        Outcome::Ignored => {}
        Outcome::Done(message) => println!("команда: {message}"),
        Outcome::Failed(message) => eprintln!("команда: {message}"),
    }
}

fn read_wav(path: &Path) -> Result<Vec<f32>, AppError> {
    let mut reader = hound::WavReader::open(path)
        .map_err(|err| AppError::Message(format!("{}: {err}", path.display())))?;
    let spec = reader.spec();
    if spec.channels != 1 || spec.sample_rate != 16_000 {
        return Err(AppError::Message(format!(
            "{}: нужен моно 16 кГц, в файле {} кан. и {} Гц",
            path.display(),
            spec.channels,
            spec.sample_rate
        )));
    }
    let samples = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| AppError::Message(err.to_string()))?,
        hound::SampleFormat::Int => {
            let scale = (1i64 << (spec.bits_per_sample.saturating_sub(1))) as f32;
            reader
                .samples::<i32>()
                .map(|sample| sample.map(|value| value as f32 / scale))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|err| AppError::Message(err.to_string()))?
        }
    };
    Ok(samples)
}

enum AppError {
    Capture(CaptureError),
    Stt(SttError),
    Message(String),
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Capture(err) => write!(f, "{err}"),
            Self::Stt(err) => write!(f, "{err}"),
            Self::Message(message) => write!(f, "{message}"),
        }
    }
}

impl From<CaptureError> for AppError {
    fn from(err: CaptureError) -> Self {
        Self::Capture(err)
    }
}

impl From<SttError> for AppError {
    fn from(err: SttError) -> Self {
        Self::Stt(err)
    }
}

fn write_wav(path: &std::path::Path, phrase: &capture::Phrase) -> Result<(), hound::Error> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: phrase.sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    for sample in &phrase.samples {
        writer.write_sample(*sample)?;
    }
    writer.finalize()
}
