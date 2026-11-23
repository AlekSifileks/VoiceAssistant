"""Локальное сравнение фразы с известными командами. Сеть открывает только первая загрузка весов.

Протокол: одна JSON-строка на запрос в stdin, одна JSON-строка на ответ в stdout.
Журнал идёт в stderr.
Запрос: {"text": "...", "options": ["курсор", "браузер"]} или {"cmd": "stop"}.
Ответ: {"ok": true, "label": "курсор", "score": 0.8} или {"ok": false, "error": "..."}.
Перед запросами одна строка {"ready": true}.
"""

import json
import os
import sys
import traceback

import numpy as np

protocol = os.fdopen(sys.stdout.fileno(), "w", buffering=1, closefd=False)
sys.stdout = sys.stderr


def reply(payload):
    protocol.write(json.dumps(payload, ensure_ascii=False) + "\n")
    protocol.flush()


def load():
    os.environ["TRANSFORMERS_VERBOSITY"] = "error"
    from sentence_transformers import SentenceTransformer

    name = os.environ.get("INTENT_MODEL", "cointegrated/rubert-tiny2").strip()
    name = name or "cointegrated/rubert-tiny2"
    model = SentenceTransformer(name)
    model.encode(["проверка"], normalize_embeddings=True)
    return model


def rank(model, text, options):
    labels = [item.strip() for item in options if isinstance(item, str) and item.strip()]
    if not text.strip() or not labels:
        raise RuntimeError("нет фразы или списка команд")
    vectors = model.encode([text, *labels], normalize_embeddings=True)
    query = np.asarray(vectors[0])
    scores = np.asarray(vectors[1:]) @ query
    order = np.argsort(scores)[::-1]
    ranked = [(labels[int(index)], float(scores[int(index)])) for index in order]
    return ranked


def serve(model):
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
        try:
            ranked = rank(model, request.get("text") or "", request.get("options") or [])
        except Exception as err:
            traceback.print_exc()
            reply({"ok": False, "error": str(err)})
            continue
        scores = [
            {"label": label, "score": round(score, 4)} for label, score in ranked
        ]
        reply(
            {
                "ok": True,
                "label": scores[0]["label"],
                "score": scores[0]["score"],
                "scores": scores,
            }
        )


def main():
    try:
        model = load()
    except Exception as err:
        traceback.print_exc()
        reply({"ready": False, "error": str(err)})
        return 1
    reply({"ready": True, "model": os.environ.get("INTENT_MODEL", "cointegrated/rubert-tiny2")})
    try:
        serve(model)
    except Exception:
        traceback.print_exc()
        return 1
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        raise SystemExit(0)
