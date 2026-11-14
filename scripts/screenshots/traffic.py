#!/usr/bin/env python3
"""Exercise the local Proxy with demo keys, including streaming and fallbacks."""
import json
import random
from collections import Counter
from concurrent.futures import ThreadPoolExecutor

import requests
from env import BASE as B, KEYS

keys = json.loads(KEYS.read_text())
random.seed(7)
PROFILES = [
    ("maria.keller", 0, 30, ["smart", "gpt-6.1-sol", "claude-opus-5-5", "claude-sonnet-5-5", "openai/gpt-6.1-sol"]),
    ("maria.keller", 1, 15, ["smart", "claude-opus-5-5"]),
    ("jonas.berg", 0, 20, ["smart", "sonnet", "gpt-4o-mini", "anthropic/claude-sonnet-5-5", "fast"]),
    ("priya.nair", 0, 25, ["smart", "fast", "text-embedding-3-small"]),
    ("lena.fischer", 0, 12, ["fast", "sonnet", "claude-sonnet-5-5"]),
    ("ci-pipeline", 0, 15, ["fast", "gpt-6-luna"]),
    ("eval-worker-03", 0, 3, ["fast"]),
    ("eval-worker-12", 0, 3, ["fast"]),
]
MSG = [{"role": "user", "content": "Summarize the quarterly report in three bullet points."}]


def one(index):
    if index >= 190:
        key = keys[f"eval-worker-{index - 189:02d}"][0]
        model, stream, anthropic = "fast", False, False
    else:
        user, ki, _, models = random.choices(PROFILES, weights=[p[2] for p in PROFILES])[0]
        key, model = keys[user][ki], random.choice(models)
        stream = random.random() < 0.4
        anthropic = random.random() < 0.4 or "claude" in model and random.random() < 0.5
    headers = {"Authorization": f"Bearer {key}"}
    if model.startswith("text-embedding"):
        endpoint, body = "/v1/embeddings", {"model": model, "input": "quarterly report"}
    else:
        endpoint = "/v1/messages" if anthropic else "/v1/chat/completions"
        body = {"model": model, "max_tokens": 2048, "stream": stream, "messages": MSG}
        if anthropic:
            headers = {"x-api-key": key, "anthropic-version": "2023-06-01"}
    response = requests.post(B + endpoint, headers=headers, json=body, timeout=90)
    assert response.status_code in (200, 429, 500, 502, 503), (response.status_code, response.text[:400])
    return response.status_code


jobs = list(range(250))
random.shuffle(jobs)
with ThreadPoolExecutor(6) as executor:
    counts = Counter(executor.map(one, jobs))
assert counts[200] > 0, counts
print(counts)
