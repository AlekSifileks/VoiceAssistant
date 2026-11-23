"""Локальный голос Qwen3-TTS. Сеть не открывает, кроме первой загрузки модели.

Протокол: одна JSON-строка на запрос в stdin, одна JSON-строка на ответ в stdout.
Журнал идёт в stderr, чтобы не смешиваться с протоколом.
"""

import json
import os
import sys
import traceback

REF_TEXT = "Привет. Меня зовут Энни, и я рада тебя слышать."
INSTRUCT = (
    "A young woman in her twenties. Warm, calm, clear voice, medium pitch. "
    "Natural conversational Russian, gentle and friendly. "
    "No announcer style, no dramatic emphasis, no whisper."
)

protocol = os.fdopen(sys.stdout.fileno(), "w", buffering=1, closefd=False)
sys.stdout = sys.stderr


def reply(payload):
    protocol.write(json.dumps(payload, ensure_ascii=False) + "\n")
    protocol.flush()


def audio_of(results):
    import numpy as np

    chunks = [np.array(result.audio).reshape(-1) for result in results]
    if not chunks:
        raise RuntimeError("модель не вернула звук")
    return np.concatenate(chunks)


def write_wav(path, audio, sample_rate):
    import soundfile as sf

    sf.write(path, audio, sample_rate)


def design_voice(design_model, voice_path, transcript_path):
    from mlx_audio.tts.utils import load_model

    print("рисую женский голос, это один раз", flush=True)
    model = load_model(design_model)
    results = list(
        model.generate_voice_design(
            text=REF_TEXT,
            language="Russian",
            instruct=INSTRUCT,
        )
    )
    audio = audio_of(results)
    voice_path.parent.mkdir(parents=True, exist_ok=True)
    write_wav(voice_path, audio, model.sample_rate)
    transcript_path.write_text(REF_TEXT + "\n", encoding="utf-8")
    del model


def main():
    voice_path = os.environ.get("TTS_VOICE", "models/annie-voice.wav")
    voice = __import__("pathlib").Path(voice_path)
    transcript = voice.with_suffix(".txt")
    design_model = os.environ.get(
        "TTS_DESIGN_MODEL",
        "mlx-community/Qwen3-TTS-12Hz-1.7B-VoiceDesign-8bit",
    )
    base_model = os.environ.get(
        "TTS_MODEL",
        "mlx-community/Qwen3-TTS-12Hz-1.7B-Base-8bit",
    )

    try:
        if not voice.is_file() or not transcript.is_file():
            design_voice(design_model, voice, transcript)
        from mlx_audio.tts.utils import load_model

        print(f"загружаю {base_model}", flush=True)
        model = load_model(base_model)
        ref_text = transcript.read_text(encoding="utf-8").strip()
    except Exception as err:
        traceback.print_exc()
        reply({"ready": False, "error": str(err)})
        return 1

    reply({"ready": True})
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            request = json.loads(line)
        except json.JSONDecodeError as err:
            reply({"ok": False, "error": f"не json: {err}"})
            continue
        if request.get("cmd") == "stop":
            break
        text = str(request.get("text", "")).strip()
        wav = request.get("wav")
        if not text or not wav:
            reply({"ok": False, "error": "нет текста или пути"})
            continue
        try:
            results = list(
                model.generate(
                    text=text,
                    ref_audio=str(voice),
                    ref_text=ref_text,
                    lang_code="Russian",
                )
            )
            write_wav(wav, audio_of(results), model.sample_rate)
            reply({"ok": True})
        except Exception as err:
            traceback.print_exc()
            reply({"ok": False, "error": str(err)})
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        raise SystemExit(0)
