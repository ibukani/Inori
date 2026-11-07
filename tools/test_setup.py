"""Fresh synthetic setup, reconfiguration, HTTPS pairing and recovery. No user data is read."""
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
    parser.add_argument("--desktop", type=Path)
    args = parser.parse_args()
    server = args.server.resolve()
    (ROOT / ".cache").mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="setup-", dir=ROOT / ".cache") as temporary:
        work = Path(temporary)
        data = work / "server data"
        with socket.socket() as reserved:
            reserved.bind(("127.0.0.1", 0))
            port = reserved.getsockname()[1]
        url = f"https://localhost:{port}"

        def command(*extra, ok=True, directory=data):
            result = subprocess.run([str(server), "--data-dir", str(directory), *map(str, extra)], capture_output=True, timeout=30)
            if ok and result.returncode:
                raise AssertionError(result.stderr.decode(errors="replace"))
            if not ok:
                assert result.returncode, "command should have failed"
            return result

        # A read-only diagnosis cannot initialize missing storage.
        command("doctor", "--offline", ok=False)
        assert not data.exists()
        setup_args = ["--listen", f"127.0.0.1:{port}", "setup", "--non-interactive", "--server-url", url]
        command(*setup_args)
        original = {name: (data / name).read_bytes() for name in ["server.pem", "server.key", "owner.token"]}
        command(*setup_args)
        assert all((data / name).read_bytes() == value for name, value in original.items())
        command("doctor", "--offline")
        # A changed SAN must not regenerate identity or overwrite saved launch settings.
        command("--listen", f"127.0.0.1:{port}", "setup", "--non-interactive", "--server-url", f"https://unpaired-name:{port}", ok=False)
        assert all((data / name).read_bytes() == value for name, value in original.items())
        assert "unpaired-name" not in (data / "launch.toml").read_text()

        with socket.socket() as occupied:
            occupied.bind(("127.0.0.1", 0))
            occupied.listen()
            taken = occupied.getsockname()[1]
            fresh = work / "port-conflict"
            command("--listen", f"127.0.0.1:{taken}", "setup", "--non-interactive", "--server-url", f"https://localhost:{taken}", directory=fresh, ok=False)
            assert not (fresh / "inori.sqlite").exists()

        offline_bundle = work / "offline.inori-connection.json"
        command("export-connection", offline_bundle, "--name", "synthetic offline client")
        bundle = json.loads(offline_bundle.read_text())
        assert bundle["app_id"] == "desktop" and "PRIVATE KEY" not in bundle["certificate_pem"]
        assert bundle["token"] != original["owner.token"].decode()
        context = ssl.create_default_context(cafile=str(data / "server.pem"))
        owner = original["owner.token"].decode()
        process = subprocess.Popen([str(server), "--data-dir", str(data)], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                   creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0)

        def call(path, token, method="GET", body=None, status=200):
            request = urllib.request.Request(url + path, data=None if body is None else json.dumps(body).encode(), method=method,
                headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"})
            try:
                with urllib.request.urlopen(request, context=context, timeout=5) as response:
                    assert status == response.status
                    return json.load(response)
            except urllib.error.HTTPError as error:
                if error.code != status:
                    raise

        try:
            deadline = time.monotonic() + 30
            while True:
                try:
                    call("/v1/capabilities", owner)
                    break
                except (OSError, urllib.error.URLError):
                    if process.poll() is not None or time.monotonic() >= deadline:
                        raise RuntimeError("setup server failed to become ready")
                    time.sleep(0.1)
            before = call("/v1/snapshot", owner)["character"]
            command(*setup_args, ok=False)  # live storage cannot be reinitialized
            online_bundle = work / "online.inori-connection.json"
            command("export-connection", online_bundle, "--name", "synthetic online client")
            online = json.loads(online_bundle.read_text())
            legacy = command("create-device", "--name", "synthetic legacy CLI")
            assert b"Device token file:" in legacy.stdout
            call("/v1/capabilities", online["token"])
            call("/v1/devices", online["token"], status=403)
            call("/v1/devices", online["token"], method="POST", body={"name": "no privilege escalation", "audience": "private", "admin": True}, status=403)
            call("/v1/devices", owner, method="POST", body={"name": "invalid public admin", "audience": "public", "admin": True}, status=400)
            command("doctor")
            if args.desktop:
                # No GUI, microphone, saved profiles, or real engines are needed.
                subprocess.run([str(args.desktop.resolve()), "--doctor", "--import-connection", str(online_bundle)], check=True, timeout=15)
            call(f"/v1/devices/{online['device_id']}", owner, method="DELETE")
            call("/v1/capabilities", online["token"], status=401)
            if args.desktop:
                invalid = subprocess.run([str(args.desktop.resolve()), "--doctor", "--import-connection", str(online_bundle)], capture_output=True, timeout=15)
                assert invalid.returncode != 0
                assert "失効" in invalid.stderr.decode("utf-8", errors="replace")
            assert call("/v1/snapshot", owner)["character"] == before
            # Verify the default distribution can produce a response without any engine or build tools.
            turn = call("/v1/chat", bundle["token"], method="POST", body={
                "text": "synthetic setup smoke test", "audience": "private", "mode": "quick",
                "session_id": "setup-smoke", "speak": False,
            })
            deadline = time.monotonic() + 10
            while True:
                sources = call("/v1/snapshot", bundle["token"])["sources"]
                history = [source for source in sources if source["turn_id"] == turn["turn_id"]]
                if len(history) == 2:
                    assert [source["actor"] for source in history] == ["user", "assistant"]
                    assert history[0]["text"] == "synthetic setup smoke test"
                    assert history[1]["text"]
                    break
                if time.monotonic() > deadline:
                    raise AssertionError("default mock response did not complete")
                time.sleep(0.05)
        finally:
            process.terminate()
            process.wait(timeout=10)
        command(*setup_args)
        assert all((data / name).read_bytes() == value for name, value in original.items())
        print(json.dumps({"status": "passed", "checks": ["fresh setup", "repeat setup", "read-only diagnosis", "certificate identity", "port conflict", "offline/online pairing", "no admin by default", "revocation", "default mock response and history order", "live lock", "restart"]}))


if __name__ == "__main__":
    main()
