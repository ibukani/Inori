"""Exercise the built server over authenticated HTTPS, restart, and restore.

Uses synthetic data and a fresh directory under .cache. Never reads the user's database.
"""
from pathlib import Path
import argparse
import json
import os
import socket
import ssl
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--server", type=Path, default=ROOT / "target/debug" / ("inori-server.exe" if os.name == "nt" else "inori-server"))
    parser.add_argument("--ollama-model", help="Optional installed local model; never pulls weights")
    args = parser.parse_args()
    (ROOT / ".cache").mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="e2e-", dir=ROOT / ".cache") as temporary:
        work = Path(temporary)
        config = ROOT / "configs/examples/extensions.toml"
        if args.ollama_model:
            contents=config.read_text(encoding="utf-8").replace('../../examples/python-extension/manifest.json',(ROOT/'examples/python-extension/manifest.json').as_posix())
            contents+='\n[[providers]]\nid="ollama-test"\nkind="ollama"\nbase_url="http://127.0.0.1:11434"\nmodel='+json.dumps(args.ollama_model)+'\nallow_private=true\nthinking=false\ntimeout_seconds=120\nmax_output_tokens=160\n'
            config=work/'server.toml'
            config.write_text(contents,encoding='utf-8')
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
        base = f"https://127.0.0.1:{port}"
        process = None
        log = (work / "server.log").open("wb")
        data = work / "original"
        context = None
        token = ""

        def command(*extra):
            return [str(args.server.resolve()), "--data-dir", str(data), "--config", str(config), *extra]
        def stop():
            nonlocal process
            if process is not None:
                process.terminate()
                process.wait(timeout=10)
                process = None
        def start():
            nonlocal process, context, token
            start_at = time.perf_counter()
            process = subprocess.Popen(command("--listen", f"127.0.0.1:{port}"), stdout=log, stderr=log,
                                       creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0)
            deadline = time.monotonic() + 30
            last = None
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    raise RuntimeError((work / "server.log").read_text(errors="replace"))
                try:
                    context = ssl.create_default_context(cafile=str(data / "server.pem"))
                    token = (data / "owner.token").read_text().strip()
                    call("GET", "/healthz")
                    return round((time.perf_counter() - start_at) * 1000, 1)
                except (OSError, ValueError, urllib.error.URLError) as error:
                    last = error
                    time.sleep(0.1)
            raise RuntimeError(f"server did not become ready: {last}")
        def call(method, path, body=None, authenticated=True):
            headers = {"Content-Type": "application/json"}
            if authenticated: headers["Authorization"] = f"Bearer {token}"
            payload = None if body is None else json.dumps(body, ensure_ascii=False).encode()
            request = urllib.request.Request(base + path, data=payload, headers=headers, method=method)
            with urllib.request.urlopen(request, context=context, timeout=10) as response:
                return json.load(response)
        def revision(): return call("GET", "/v1/snapshot")["character"]["revision"]
        def expect_status(status, *a, **kw):
            try: call(*a, **kw)
            except urllib.error.HTTPError as error:
                assert error.code == status, (error.code, status)
            else: raise AssertionError(f"expected {status}")

        try:
            initial_ms = start()
            expect_status(401, "GET", "/v1/snapshot", authenticated=False)
            original = call("POST", "/v1/memories", {"text":"架空の好み：紅茶", "audience":"private", "expected_revision":revision()})
            current = call("PUT", f"/v1/memories/{original['id']}", {"text":"架空の好み：珈琲", "expected_revision":revision()})
            expect_status(409, "PUT", f"/v1/memories/{current['id']}", {"text":"古い結果", "expected_revision":0})
            operation = {"id":"synthetic-once", "extension":"example.sensor", "capability":"example.sensor.pose", "action":"invoke", "target":"server", "arguments":{}}
            expect_status(403, "POST", "/v1/operations", operation)
            call("PUT", "/v1/grants", {**{key:operation[key] for key in ["extension","capability","action","target"]}, "allowed":True})
            assert call("POST", "/v1/operations", operation)["result"]["pose"] == [0.25, 0.5, 0.0]
            expect_status(409, "POST", "/v1/operations", operation)
            call("PUT", "/v1/grants", {"extension":"example.sensor","capability":"example.inference","action":"infer","target":"server","allowed":True})
            call("PUT", "/v1/providers/active", {"provider":"python-demo", "expected_revision":revision()})
            accepted = call("POST", "/v1/chat", {"text":"こんにちは", "audience":"private", "mode":"auto", "session_id":"synthetic", "speak":False})
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                snapshot = call("GET", "/v1/snapshot")
                if any(s["turn_id"] == accepted["turn_id"] and s["actor"] == "assistant" for s in snapshot["sources"]): break
                time.sleep(0.05)
            else: raise AssertionError("extension inference never completed")
            backup = work / "before-delete.sqlite"
            subprocess.run(command("backup", str(backup)), check=True, stdout=subprocess.DEVNULL, timeout=15)
            call("DELETE", f"/v1/memories/{current['id']}", {"expected_revision":revision()})
            stop()
            restart_ms = start()
            assert not call("GET", "/v1/snapshot")["memories"], "deleted memory returned after restart"
            stop()
            data = work / "restored"
            subprocess.run(command("restore", str(backup)), check=True, stdout=subprocess.DEVNULL, timeout=15)
            # Restore contains existing hashed credentials. Provision a NEW recovery credential.
            provision = subprocess.run(command("create-device", "--name", "recovery", "--admin"), check=True, stdout=subprocess.PIPE, text=True, timeout=15)
            device_path = Path(next(line.removeprefix("Device token file: ") for line in provision.stdout.splitlines() if line.startswith("Device token file: ")))
            (data / "owner.token").write_bytes(device_path.read_bytes())
            start()
            restored = call("GET", "/v1/snapshot")
            assert restored["memories"][0]["text"] == "架空の好み：珈琲"
            assert restored["character"]["active_provider"] == "python-demo"
            llm_ms=None
            if args.ollama_model:
                call("PUT","/v1/providers/active",{"provider":"ollama-test","expected_revision":revision()})
                at=time.perf_counter()
                accepted=call("POST","/v1/chat",{"text":"日本語で一言だけ挨拶してください。","audience":"private","mode":"quick","session_id":"synthetic","speak":False})
                deadline=time.monotonic()+130
                while time.monotonic()<deadline:
                    snapshot=call("GET","/v1/snapshot")
                    if any(s["turn_id"]==accepted["turn_id"] and s["actor"]=="assistant" and s["text"].strip() for s in snapshot["sources"]):
                        llm_ms=round((time.perf_counter()-at)*1000,1);break
                    time.sleep(.1)
                else: raise AssertionError("local Ollama response did not complete")
            print(json.dumps({"status":"passed", "transport":"HTTPS + explicit certificate", "initial_ready_ms":initial_ms, "restart_ready_ms":restart_ms,"optional_ollama_model":args.ollama_model,"optional_full_text_ms":llm_ms, "checked":["authentication","stale revision","memory correction","extension permissions","duplicate operation","RPC model replacement","delete suppression after restart","backup restore"]}, ensure_ascii=False, indent=2))
        finally:
            stop()
            log.close()

if __name__ == "__main__": main()
