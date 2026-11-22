//! Набросок приёма голоса.
//!
//! Слышит микрофон, закрывает фразу по тишине и пишет её в wav.
//! Распознавания и отправки текста нет. Граница модуля — `docs/modules/audio-capture.md`.

mod capture;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use capture::{CaptureError, Config, Event};

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), CaptureError> {
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

    std::fs::create_dir_all(&args.out).map_err(|err| CaptureError::Device(err.to_string()))?;

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
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        probe: false,
        out: PathBuf::from("captures"),
        threshold: Config::default().threshold,
        silence_ms: Config::default().silence_ms,
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
voice-assistant — слушает микрофон и пишет фразы в wav. Никуда не отправляет.

  cargo run --
  cargo run -- --probe
  cargo run -- --threshold 0.02 --silence-ms 800 --out captures

  --probe           секунда с микрофона и уровень, без записи
  --out DIR         куда писать phrase-NNN.wav (по умолчанию captures)
  --threshold N     уровень, с которого кадр считается речью (по умолчанию 0.02)
  --silence-ms N    пауза, которая закрывает фразу (по умолчанию 700)"
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
