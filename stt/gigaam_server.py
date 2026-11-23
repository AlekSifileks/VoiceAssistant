"""Локальное распознавание GigaAM v3. Сеть открывает только первая загрузка весов.

Протокол: одна JSON-строка на запрос в stdin, одна JSON-строка на ответ в stdout.
Журнал идёт в stderr, чтобы не смешиваться с протоколом.
Запрос: {"wav": "/путь/фраза.wav"} или {"cmd": "stop"}.
Ответ: {"ok": true, "text": "..."} или {"ok": false, "error": "..."}.
Перед запросами одна строка {"ready": true}.
"""

import json
import os
import sys
import traceback
import wave

import numpy as np

protocol = os.fdopen(sys.stdout.fileno(), "w", buffering=1, closefd=False)
sys.stdout = sys.stderr

MIN_SAMPLES = 320


def reply(payload):
    protocol.write(json.dumps(payload, ensure_ascii=False) + "\n")
    protocol.flush()


def read_wav(path):
    with wave.open(path, "rb") as reader:
        channels = reader.getnchannels()
        rate = reader.getframerate()
        width = reader.getsampwidth()
        frames = reader.readframes(reader.getnframes())
    if channels != 1 or rate != 16000:
        raise RuntimeError(f"нужен моно 16 кГц, в файле {channels} кан. и {rate} Гц")
    if width == 2:
        audio = np.frombuffer(frames, dtype="<i2").astype(np.float32) / 32768.0
    elif width == 4:
        audio = np.frombuffer(frames, dtype="<f4").astype(np.float32)
    else:
        raise RuntimeError(f"неизвестная глубина wav: {width * 8} бит")
    return np.ascontiguousarray(audio)


def load():
    from gigaam_mlx import load_model

    raw = os.environ.get("STT_MODEL", "rnnt").strip() or "rnnt"
    if raw in ("rnnt", "ctc"):
        return load_model(raw)
    kind = "ctc" if "ctc" in raw.lower() else "rnnt"
    return load_model(kind, repo_id=raw)


def transcribe(model, tokenizer, audio):
    import mlx.core as mx
    from gigaam_mlx.audio import compute_mel, split_audio

    if audio.size < MIN_SAMPLES:
        return ""
    parts = []
    for chunk in split_audio(audio):
        piece = audio[chunk["start_sample"] : chunk["end_sample"]]
        if piece.size < MIN_SAMPLES:
            continue
        mel = compute_mel(piece)
        if mel.shape[0] == 0:
            continue
        encoded, seq_len = model.encode(mx.array(mel[np.newaxis]))
        mx.eval(encoded)
        text = tokenizer.decode(model.decode(encoded, seq_len)).strip()
        if text:
            parts.append(text)
    return " ".join(parts)


def serve(model, tokenizer):
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
            return
        path = request.get("wav")
        if not path:
            reply({"ok": False, "error": "нет пути wav"})
            continue
        try:
            text = transcribe(model, tokenizer, read_wav(path))
        except Exception as err:
            traceback.print_exc()
            reply({"ok": False, "error": str(err)})
            continue
        reply({"ok": True, "text": text})


def main():
    try:
        model, tokenizer = load()
    except Exception as err:
        traceback.print_exc()
        reply({"ready": False, "error": str(err)})
        return 1
    reply({"ready": True})
    try:
        serve(model, tokenizer)
    except Exception:
        traceback.print_exc()
        return 1
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        raise SystemExit(0)
