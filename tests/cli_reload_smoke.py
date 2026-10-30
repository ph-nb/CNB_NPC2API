"""Verify the released server's CLI configuration reload over HTTP."""

import json
import os
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path


def unused_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def main():
    name = "cnb-npc2api.exe" if os.name == "nt" else "cnb-npc2api"
    binary = Path(os.environ.get("CNB_NPC2API_BINARY") or os.environ.get("CNB_FORGE_BINARY") or str(Path("target/release") / name)).resolve()
    label, separator, version = subprocess.check_output([str(binary), "--version"], text=True).strip().partition(" ")
    if label != "CNB_NPC2API" or not separator or not version:
        raise RuntimeError("gateway returned an invalid version")
    port = unused_port()
    base = f"http://127.0.0.1:{port}"
    config = {"server": {"host": "127.0.0.1", "port": 19000}, "client_authentication": {"enabled": True, "allowed_keys": ["old-test-key"]}, "upstreams": [{"name": "mock", "type": "openai_compat", "base_url": "http://127.0.0.1:9/v1", "models": ["old-model"], "native_fc": True}]}
    with tempfile.TemporaryDirectory(prefix="cnb-npc2api-reload-") as directory:
        path = Path(directory) / "config.yaml"
        path.write_text(json.dumps(config), encoding="utf-8")
        process = subprocess.Popen([str(binary), "--config", str(path), "--host", "127.0.0.1", "--port", str(port), "--reload"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
        try:
            def models(key):
                request = urllib.request.Request(base + "/v1/models", headers={"Authorization": "Bearer " + key})
                try:
                    with urllib.request.urlopen(request, timeout=1) as response:
                        return response.status, json.load(response)
                except urllib.error.HTTPError as error:
                    return error.code, None
                except OSError:
                    return 0, None

            def wait_for(predicate):
                for _ in range(100):
                    if process.poll() is not None:
                        raise RuntimeError("gateway exited during reload test")
                    if predicate():
                        return
                    time.sleep(0.1)
                raise RuntimeError("gateway reload did not reach the expected state")

            wait_for(lambda: models("old-test-key")[0] == 200)
            with urllib.request.urlopen(base + "/healthz", timeout=1) as response:
                if json.load(response).get("version") != version:
                    raise RuntimeError("health and CLI versions differ")
            pid = process.pid
            config["client_authentication"]["allowed_keys"] = ["new-test-key"]
            config["upstreams"][0]["models"] = ["new-model"]
            config["server"]["port"] = 19001
            path.write_text(json.dumps(config), encoding="utf-8")
            wait_for(lambda: models("new-test-key")[0] == 200)
            status, result = models("new-test-key")
            assert status == 200 and result["data"][0]["id"] == "new-model"
            assert models("old-test-key")[0] == 401 and process.pid == pid
            path.write_text("server: [", encoding="utf-8")
            time.sleep(1.5)
            assert models("new-test-key")[0] == 200 and process.poll() is None
            print("CLI version and health agree; changed key and model applied; invalid config retained previous state")
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


if __name__ == "__main__":
    main()
