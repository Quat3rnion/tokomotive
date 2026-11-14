#!/usr/bin/env python3
"""Local OpenAI and Anthropic upstream with demo usage and occasional errors."""
import json
import os
import random
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def send_json(self, code, obj, extra=None):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        for key, value in (extra or {}).items():
            self.send_header(key, value)
        self.end_headers()
        self.wfile.write(body)

    def start_stream(self):
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.send_header("connection", "close")
        self.end_headers()
        self.close_connection = True

    def write(self, text):
        self.wfile.write(text.encode())
        self.wfile.flush()

    def do_GET(self):
        self.send_json(200, {"object": "list", "data": [{"id": "gpt-6.1-sol", "object": "model"}]})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("content-length") or 0)) or b"{}")
        if self.path == "/mcp":
            method = body.get("method")
            if method == "notifications/initialized":
                self.send_response(202)
                self.send_header("content-length", "0")
                self.end_headers()
                return
            if method == "initialize":
                result = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "Demo tools", "version": "1"}}
            elif method == "tools/list":
                result = {"tools": [
                    {"name": "search_docs", "description": "Search the product documentation.", "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}}}},
                    {"name": "read_ticket", "description": "Read a support ticket by its number.", "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}}},
                ]}
            else:
                result = {"content": [{"type": "text", "text": "Demo result"}]}
            return self.send_json(200, {"jsonrpc": "2.0", "id": body.get("id"), "result": result}, {"mcp-session-id": "demo-mcp-session"})
        model = body.get("model", "unknown")
        stream = bool(body.get("stream"))
        anthropic = self.path.endswith("/messages")
        rate = 0.18 if "opus" in model else 0.05
        chance = random.random()
        time.sleep(random.uniform(0.05, 0.25))
        if chance < rate * 0.6:
            return self.send_json(429, {"error": {"type": "rate_limit_error", "message": "rate limited"}}, {"retry-after": "1"})
        if chance < rate:
            return self.send_json(500, {"error": {"type": "api_error", "message": "internal error"}})
        big = "opus" in model or "sol" in model
        inp = random.randint(300, 6000) if big else random.randint(80, 2500)
        out = random.randint(60, 1400) if big else random.randint(20, 600)
        cached = random.choice([0, 0, random.randint(200, 3000)])
        if self.path.endswith("/embeddings"):
            return self.send_json(200, {"object": "list", "model": model, "data": [{"object": "embedding", "index": 0, "embedding": [0.01] * 8}], "usage": {"prompt_tokens": inp, "total_tokens": inp}})
        text = "Here is a concise answer to your question."
        if anthropic:
            cw5 = random.choice([0, 0, 0, random.randint(100, 1500)])
            cw1 = random.choice([0, 0, 0, 0, random.randint(500, 4000)])
            usage = {"input_tokens": inp, "output_tokens": out, "cache_read_input_tokens": cached, "cache_creation_input_tokens": cw5 + cw1, "cache_creation": {"ephemeral_5m_input_tokens": cw5, "ephemeral_1h_input_tokens": cw1}}
            mid = "msg_%08x" % random.getrandbits(32)
            message = {"id": mid, "type": "message", "role": "assistant", "model": model, "content": [{"type": "text", "text": text}], "stop_reason": "end_turn", "stop_sequence": None, "usage": usage}
            if not stream:
                time.sleep(out / random.uniform(800, 2500))
                return self.send_json(200, message)
            self.start_stream()

            def event(kind, data):
                self.write(f"event: {kind}\ndata: {json.dumps(data)}\n\n")

            time.sleep(random.uniform(0.15, 0.6))
            event("message_start", {"type": "message_start", "message": dict(message, content=[], stop_reason=None, usage=dict(usage, output_tokens=1))})
            event("content_block_start", {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})
            for word in text.split():
                time.sleep(out / random.uniform(800, 2500) / 8)
                event("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": word + " "}})
            event("content_block_stop", {"type": "content_block_stop", "index": 0})
            event("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None}, "usage": {"output_tokens": out}})
            event("message_stop", {"type": "message_stop"})
            return
        usage = {"prompt_tokens": inp + cached, "completion_tokens": out, "total_tokens": inp + cached + out, "prompt_tokens_details": {"cached_tokens": cached}}
        base = {"id": "chatcmpl-%08x" % random.getrandbits(32), "created": int(time.time()), "model": model}
        if not stream:
            time.sleep(out / random.uniform(800, 2500))
            return self.send_json(200, dict(base, object="chat.completion", choices=[{"index": 0, "message": {"role": "assistant", "content": text}, "finish_reason": "stop"}], usage=usage))
        self.start_stream()
        time.sleep(random.uniform(0.15, 0.6))
        base["object"] = "chat.completion.chunk"
        for word in text.split():
            time.sleep(out / random.uniform(800, 2500) / 8)
            self.write("data: " + json.dumps(dict(base, choices=[{"index": 0, "delta": {"content": word + " "}, "finish_reason": None}])) + "\n\n")
        self.write("data: " + json.dumps(dict(base, choices=[{"index": 0, "delta": {}, "finish_reason": "stop"}])) + "\n\n")
        self.write("data: " + json.dumps(dict(base, choices=[], usage=usage)) + "\n\n")
        self.write("data: [DONE]\n\n")


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", int(os.environ.get("SHOTS_MOCK_PORT", "18832"))), Handler).serve_forever()
