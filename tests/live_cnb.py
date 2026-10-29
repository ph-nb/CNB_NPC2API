"""Run two small requests through the real CNB upstream."""

import argparse
import getpass
import json
import os
import secrets
import socket
import subprocess
import sys
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
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--session-prompt", action="store_true", help="read the Cookie header without echoing it")
    parser.add_argument("--state", action="store_true", help="also test Responses conversation and tool-result history")
    parser.add_argument("--state-only", action="store_true", help="test only Responses conversation and tool-result history")
    args = parser.parse_args()
    executable = "cnb-forge.exe" if os.name == "nt" else "cnb-forge"
    release_binary = Path("target/release") / executable
    default_binary = str(release_binary if release_binary.is_file() else Path("target/debug") / executable)
    binary = Path(os.environ.get("CNB_FORGE_BINARY", default_binary)).resolve()
    if not binary.is_file():
        raise FileNotFoundError(binary)
    port = unused_port()
    base = f"http://127.0.0.1:{port}"
    key = secrets.token_urlsafe(32)
    with tempfile.TemporaryDirectory(prefix="cnb-forge-live-") as tmp:
        config = Path(tmp) / "config.yaml"
        config.write_text(
            f"server:\n  host: 127.0.0.1\n  port: {port}\n"
            f"client_authentication:\n  enabled: true\n  allowed_keys: [{json.dumps(key)}]\n"
            "upstreams:\n  - name: cnb\n    type: cnb\n    base_url: https://cnb.cool\n    session_cookie: '${CNB_SESSION_COOKIE}'\n"
            "    models: [deepseek-v4-flash]\n    native_fc: false\n    pool_min: 1\n    pool_max: 2\n",
            encoding="utf-8",
        )
        env = os.environ.copy()
        if args.session_prompt:
            env["CNB_SESSION_COOKIE"] = getpass.getpass("CNB Cookie: ")
        env["RUST_LOG"] = "cnb_forge=warn"
        process = subprocess.Popen([str(binary), "--config", str(config)], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, env=env,
                                   creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
        try:
            for _ in range(300):
                if process.poll() is not None:
                    raise RuntimeError("gateway startup failed: " + process.stderr.read().decode(errors="replace").strip())
                try:
                    with urllib.request.urlopen(base + "/healthz", timeout=1) as response:
                        if response.status == 200:
                            break
                except OSError:
                    time.sleep(0.1)
            else:
                raise RuntimeError("gateway did not start")

            def post(body, path="/v1/chat/completions"):
                request = urllib.request.Request(base + path, data=json.dumps(body).encode(),
                                                 headers={"Authorization": "Bearer " + key, "Content-Type": "application/json"})
                try:
                    return urllib.request.urlopen(request, timeout=120)
                except urllib.error.HTTPError as error:
                    data = json.load(error)
                    message = data.get("error", {}).get("message", "request failed")
                    raise RuntimeError(f"gateway returned HTTP {error.code}: {message}") from error

            tool = {"type": "function", "function": {"name": "get_weather", "description": "Get the weather for a city",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}}}
            if not args.state_only:
                with post({"model": "deepseek-v4-flash", "stream": True, "max_tokens": 64,
                           "messages": [{"role": "user", "content": "What is 2+2? Answer briefly."}]}) as response:
                    content = []
                    done = False
                    for raw in response:
                        line = raw.decode().strip()
                        if line == "data: [DONE]":
                            done = True
                            break
                        if line.startswith("data: {"):
                            event = json.loads(line[6:])
                            content.append(event.get("choices", [{}])[0].get("delta", {}).get("content", ""))
                    if not done or not "".join(content).strip():
                        raise RuntimeError("live CNB stream did not complete with text")
                    print(f"Live CNB text stream: completed; characters={len(''.join(content))}")

                with post({"model": "deepseek-v4-flash", "stream": False, "max_tokens": 512, "temperature": 0,
                           "messages": [{"role": "user", "content": "Use the get_weather tool for Tokyo."}], "tools": [tool]}) as response:
                    result = json.load(response)
                    calls = result["choices"][0]["message"].get("tool_calls") or []
                    if not calls or calls[0]["function"]["name"] != "get_weather":
                        raise RuntimeError("live CNB did not emit the requested parseable tool call")
                    tool_args = json.loads(calls[0]["function"]["arguments"])
                    if not tool_args.get("city"):
                        raise RuntimeError("live CNB tool call omitted the city")
                    print("Live CNB prompt tool call: parsed with the required city argument")

            if args.state or args.state_only:
                with post({"model": "deepseek-v4-flash", "input": "Remember the exact marker BLUE-42 for my next question. Reply briefly.", "max_output_tokens": 64}, "/v1/responses") as response:
                    first = json.load(response)
                first_id = first["id"]
                with post({"model": "deepseek-v4-flash", "previous_response_id": first_id, "input": "What marker did I ask you to remember? Reply with the marker only.", "max_output_tokens": 64, "stream": True}, "/v1/responses") as response:
                    events = [json.loads(line.decode()[6:]) for line in response if line.startswith(b"data: {")]
                completed = next((event["response"] for event in events if event.get("type") == "response.completed"), None)
                if not completed:
                    raise RuntimeError("live Responses stream did not complete")
                text = "".join(part.get("text", "") for item in completed.get("output", []) for part in item.get("content", []))
                if "BLUE-42" not in text.upper():
                    raise RuntimeError("live Responses continuation did not preserve the marker")
                print("Live Responses conversation: streamed continuation retained context")

                responses_tool = {"type": "function", "name": "get_weather", "description": "Get weather for a city", "parameters": tool["function"]["parameters"]}
                with post({"model": "deepseek-v4-flash", "input": "Use get_weather for Tokyo and then report the result exactly.", "max_output_tokens": 512, "tools": [responses_tool], "tool_choice": "required"}, "/v1/responses") as response:
                    first_tool = json.load(response)
                call = next((item for item in first_tool["output"] if item["type"] == "function_call"), None)
                if not call:
                    raise RuntimeError("live Responses did not return the required tool call")
                with post({"model": "deepseek-v4-flash", "previous_response_id": first_tool["id"], "input": [{"type": "function_call_output", "call_id": call["call_id"], "output": "FORECAST_MARKER_8"}], "max_output_tokens": 128}, "/v1/responses") as response:
                    second_tool = json.load(response)
                text = "".join(part.get("text", "") for item in second_tool.get("output", []) for part in item.get("content", []))
                if "FORECAST_MARKER_8" not in text.upper():
                    raise RuntimeError("live Responses continuation did not preserve the tool result")
                print("Live Responses tool-result continuation: completed")
        finally:
            if process.poll() is None:
                process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            diagnostics = process.stderr.read().decode(errors="replace").strip()
            if diagnostics:
                print("Gateway diagnostics:", diagnostics)


if __name__ == "__main__":
    try:
        main()
    except RuntimeError as error:
        print(f"Live CNB check failed: {error}", file=sys.stderr)
        raise SystemExit(1) from None
