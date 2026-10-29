"""Verify container startup and authentication without contacting real upstreams."""

import json
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path


def docker(*args):
    return subprocess.run(["docker", *args], check=True, capture_output=True, text=True).stdout.strip()


def unused_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def request(url, key=None):
    headers = {"Authorization": "Bearer " + key} if key else {}
    try:
        with urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=2) as response:
            return response.status, json.load(response)
    except urllib.error.HTTPError as error:
        return error.code, None


def check_image(image):
    user = docker("image", "inspect", "--format", "{{.Config.User}}", image)
    if user in ("", "root", "0", "0:0"):
        raise RuntimeError(f"{image} does not configure a non-root user")
    name = "cnb-forge-smoke-" + uuid.uuid4().hex[:12]
    port = unused_port()
    base = f"http://127.0.0.1:{port}"
    config = {
        "server": {"host": "127.0.0.1", "port": 8080},
        "client_authentication": {"enabled": True, "allowed_keys": ["docker-test-key"]},
        "upstreams": [{"name": "mock", "type": "openai_compat", "base_url": "http://127.0.0.1:9/v1", "models": ["test-model"]}],
    }
    with tempfile.TemporaryDirectory(prefix="cnb-forge-docker-") as directory:
        path = Path(directory) / "config.yaml"
        path.write_text(json.dumps(config), encoding="utf-8")
        path.chmod(0o644)
        try:
            docker("run", "--detach", "--rm", "--name", name, "--publish", f"127.0.0.1:{port}:8080", "--mount", f"type=bind,source={path},target=/app/smoke.yaml,readonly", image, "--config", "/app/smoke.yaml")
            for _ in range(150):
                try:
                    status, health = request(base + "/healthz")
                    if status == 200 and health.get("status") == "ok":
                        break
                except (OSError, ValueError):
                    pass
                time.sleep(0.2)
            else:
                raise RuntimeError(f"{image} did not become healthy")
            if request(base + "/v1/models")[0] != 401:
                raise RuntimeError(f"{image} allowed an unauthenticated models request")
            status, models = request(base + "/v1/models", "docker-test-key")
            if status != 200 or not any(item.get("id") == "test-model" for item in models.get("data", [])):
                raise RuntimeError(f"{image} returned an unexpected model list")
            print(f"{image}: non-root, healthy, authentication and models passed")
        finally:
            subprocess.run(["docker", "stop", name], capture_output=True, text=True, check=False)


if __name__ == "__main__":
    if len(sys.argv) < 2:
        raise SystemExit("Usage: python tests/docker_smoke.py IMAGE [IMAGE ...]")
    for image in sys.argv[1:]:
        check_image(image)
