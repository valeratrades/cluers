#!/usr/bin/env python3
"""Mock STT + OpenAI-style SSE chat. Every request is appended to $LOG as JSON lines."""
import array, io, json, math, os, sys, threading, time, wave
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOG = os.environ["LOG"]
lock = threading.Lock()
n = {"stt": 0, "chat": 0}


def log(rec):
    rec["t"] = time.time()
    with lock, open(LOG, "a") as f:
        f.write(json.dumps(rec) + "\n")


def wav_secs(body):
    i = body.find(b"RIFF")
    assert i >= 0, "no WAV in STT body"
    with wave.open(io.BytesIO(body[i:])) as w:
        assert w.getsampwidth() == 2 and w.getnchannels() == 1, "app sends mono s16"
        a = array.array("h", w.readframes(w.getnframes()))
        rms = math.sqrt(sum(x * x for x in a) / max(len(a), 1)) / 32768
        return w.getnframes() / w.getframerate(), w.getframerate(), rms


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def body(self):
        return self.rfile.read(int(self.headers.get("Content-Length", 0)))

    def reply(self, code, ctype, data):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):
        b = self.body()
        if self.path.startswith("/stt"):
            with lock:
                n["stt"] += 1
                k = n["stt"]
            secs, sr, rms = wav_secs(b)
            text = f"question{k} lasting {secs:.1f} seconds"
            log({"kind": "stt", "n": k, "secs": round(secs, 2), "sr": sr, "rms": round(rms, 4), "text": text})
            return self.reply(200, "application/json", json.dumps({"text": text}).encode())
        if self.path.startswith("/v1/chat/completions"):
            req = json.loads(b)
            with lock:
                n["chat"] += 1
                k = n["chat"]
            last = req["messages"][-1]["content"]
            if isinstance(last, list):
                last = " ".join(p.get("text", "") for p in last if isinstance(p, dict))
            log({"kind": "chat", "n": k, "messages": req["messages"], "stream": req.get("stream")})
            words = f"ANSWER{k} to [{last[-80:]}]".split(" ")
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()

            def chunk(s):
                d = s.encode()
                self.wfile.write(b"%x\r\n%s\r\n" % (len(d), d))
                self.wfile.flush()

            for i, w in enumerate(words):
                delta = {"choices": [{"delta": {"content": (" " if i else "") + w}}]}
                chunk(f"data: {json.dumps(delta)}\n\n")
                time.sleep(0.05)
            chunk("data: [DONE]\n\n")
            self.wfile.write(b"0\r\n\r\n")
            return
        log({"kind": "unknown", "path": self.path})
        self.reply(404, "text/plain", b"no")


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
