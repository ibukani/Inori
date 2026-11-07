"""Validate synthetic saved-key injection and environment precedence against a local fake engine."""
from pathlib import Path
import argparse
import http.server
import json
import os
import socket
import ssl
import subprocess
import tempfile
import threading
import time
import tomllib
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--server", type=Path, default=ROOT / "target/debug" / ("inori-server.exe" if os.name == "nt" else "inori-server"))
    args = parser.parse_args()
    observed = []

    class Engine(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            observed.append(self.headers.get("Authorization"))
            body = b'{"data":[{"id":"synthetic-model"}]}'
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            self.rfile.read(int(self.headers.get("Content-Length", 0)))
            observed.append(self.headers.get("Authorization"))
            body = b'data: {"choices":[{"delta":{"content":"synthetic reply"}}]}\n\ndata: [DONE]\n\n'
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_):
            pass

    engine = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Engine)
    thread = threading.Thread(target=engine.serve_forever, daemon=True)
    thread.start()
    try:
        (ROOT / ".cache").mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="setup-credentials-", dir=ROOT / ".cache") as temporary:
            work = Path(temporary).resolve()
            data = work / "data"
            key_file = work / "synthetic.key"
            key_file.write_text("synthetic-saved-key")
            with socket.socket() as sock:
                sock.bind(("127.0.0.1", 0))
                port = sock.getsockname()[1]
            url = f"https://localhost:{port}"
            executable = str(args.server.resolve())
            command = [executable, "--data-dir", str(data)]
            clean_env = dict(os.environ)
            clean_env.pop("SYNTHETIC_INORI_SETUP_KEY", None)
            setup = [*command, "--listen", f"127.0.0.1:{port}", "setup", "--non-interactive", "--server-url", url,
                     "--provider", "compatible_http", "--provider-url", f"http://127.0.0.1:{engine.server_port}",
                     "--model", "synthetic-model", "--api-key-env", "SYNTHETIC_INORI_SETUP_KEY", "--api-key-file", str(key_file), "--allow-private"]
            result = subprocess.run(setup, env=clean_env, check=True, capture_output=True, timeout=30)
            assert b"synthetic-saved-key" not in result.stdout + result.stderr
            assert "synthetic-saved-key" not in (data / "server.toml").read_text(encoding="utf-8")
            assert (data / "server.secrets.toml").is_file()
            context = ssl.create_default_context(cafile=str(data / "server.pem"))
            owner = (data / "owner.token").read_text().strip()

            def call(path, body=None):
                request = urllib.request.Request(url + path, data=None if body is None else json.dumps(body).encode(),
                    headers={"Authorization": f"Bearer {owner}", "Content-Type": "application/json"})
                with urllib.request.urlopen(request, context=context, timeout=5) as response:
                    return json.load(response)

            active = None
            for override, expected in [(None, "Bearer synthetic-saved-key"), ("synthetic-override-key", "Bearer synthetic-override-key")]:
                env = dict(clean_env)
                if override:
                    env["SYNTHETIC_INORI_SETUP_KEY"] = override
                process = subprocess.Popen(command, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                    creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0)
                try:
                    deadline = time.monotonic() + 30
                    while True:
                        try:
                            snapshot = call("/v1/snapshot")
                            break
                        except (OSError, urllib.error.URLError):
                            if process.poll() is not None or time.monotonic() > deadline:
                                raise RuntimeError("synthetic server failed to start")
                            time.sleep(0.1)
                    active = snapshot["character"]["active_provider"]
                    assert active == "setup-ai"
                    start = len(observed)
                    turn = call("/v1/chat", {"text":"synthetic setup input", "audience":"private", "mode":"quick", "session_id":"synthetic-setup", "speak":False})
                    deadline = time.monotonic() + 10
                    while not any(source["turn_id"] == turn["turn_id"] and source["actor"] == "assistant" for source in call("/v1/snapshot")["sources"]):
                        if time.monotonic() > deadline:
                            raise AssertionError("saved-key inference failed")
                        time.sleep(0.05)
                    assert observed[start:] and all(value == expected for value in observed[start:])
                    subprocess.run([*command, "doctor", "--engines"], env=env, check=True, capture_output=True, timeout=15)
                finally:
                    process.terminate()
                    process.wait(timeout=10)
            second = list(setup)
            second[second.index("synthetic-model")] = "synthetic-other-model"
            subprocess.run(second, env=clean_env, check=True, capture_output=True, timeout=30)
            config = tomllib.loads((data / "server.toml").read_text(encoding="utf-8"))
            assert next(p for p in config["providers"] if p["id"] == active)["model"] == "synthetic-model"
            assert any(p["model"] == "synthetic-other-model" and p["id"] != active for p in config["providers"])
            print("saved credentials, environment precedence, metadata checks, and model preservation: passed")
    finally:
        engine.shutdown()
        engine.server_close()
        thread.join(timeout=5)


if __name__ == "__main__":
    main()
