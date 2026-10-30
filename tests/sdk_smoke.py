"""Exercise official client SDKs against a local CNB protocol fixture."""

import argparse
import getpass
import json
import os
import secrets
import socket
import subprocess
import tempfile
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from anthropic import Anthropic
from google import genai
from google.genai import types
from openai import OpenAI

KEY = "local-test-key"
MODEL = "deepseek-v4-flash"
TOKEN = secrets.token_hex(20)
COOKIE = secrets.token_hex(16)
SCHEMA = {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}
TOOL_TEXT = (
    '<|XYML|tool_calls><|XYML|invoke name="get_weather">'
    '<|XYML|parameter name="city"><![CDATA[Tokyo]]></|XYML|parameter>'
    '</|XYML|invoke></|XYML|tool_calls>'
)


class MockCnb(BaseHTTPRequestHandler):
    calls = []

    def log_message(self, *args):
        pass

    def do_GET(self):
        body = ('<script>window.csrftoken="' + TOKEN + '"</script>').encode()
        self.send_response(200)
        self.send_header("Set-Cookie", "csrfkey=" + COOKIE + "; Path=/; HttpOnly")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        assert self.path == "/ai/chat/completions"
        assert self.headers.get("csrftoken") == TOKEN
        assert self.headers.get("cookie") == "csrfkey=" + COOKIE
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        assert "tools" not in request
        self.calls.append(request)
        text = TOOL_TEXT if "TOOL CALL PROTOCOL" in json.dumps(request) else "hello from mock"
        events = [
            {"id": "upstream", "model": MODEL, "choices": [{"delta": {"content": text}, "finish_reason": None}]},
            {"id": "upstream", "model": MODEL, "choices": [{"delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}},
        ]
        body = "".join("data: " + json.dumps(e) + "\n\n" for e in events) + "data: [DONE]\n\n"
        data = body.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        for i in range(0, len(data), 11):
            self.wfile.write(data[i:i + 11])
        self.wfile.flush()


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_ready(base, process):
    for _ in range(100):
        if process.poll() is not None:
            raise RuntimeError("gateway exited before becoming ready")
        try:
            with urllib.request.urlopen(base + "/healthz", timeout=1) as response:
                if response.status == 200:
                    return
        except OSError:
            time.sleep(0.1)
    raise RuntimeError("gateway did not start")


def prompt(use_tools, live):
    if not live:
        return "Weather?"
    return "Use the get_weather tool for Tokyo." if use_tools else "What is 2+2? Answer briefly."


def check_text(text, live):
    assert text.strip() if live else text == "hello from mock"


def check_city(args, live):
    assert args.get("city") if live else args.get("city") == "Tokyo"


def test_openai(base, live=False):
    client = OpenAI(api_key=KEY, base_url=base + "/v1", timeout=120 if live else 10, max_retries=0)
    tools = [{"type": "function", "function": {"name": "get_weather", "parameters": SCHEMA}}]
    for stream in (False, True):
        for use_tools in (False, True):
            print(f"Checking chat stream={stream} tools={use_tools}", flush=True)
            kwargs = {"model": MODEL, "messages": [{"role": "user", "content": prompt(use_tools, live)}], "stream": stream}
            if live:
                kwargs.update(max_tokens=512 if use_tools else 64, temperature=0)
            if use_tools:
                kwargs["tools"] = tools
            response = client.chat.completions.create(**kwargs)
            if stream:
                chunks = list(response)
                if use_tools:
                    calls = [tc for chunk in chunks for choice in chunk.choices for tc in (choice.delta.tool_calls or [])]
                    assert calls[0].function.name == "get_weather"
                    check_city(json.loads(calls[0].function.arguments), live)
                else:
                    check_text("".join(choice.delta.content or "" for chunk in chunks for choice in chunk.choices), live)
            elif use_tools:
                assert response.choices[0].message.tool_calls[0].function.name == "get_weather"
                check_city(json.loads(response.choices[0].message.tool_calls[0].function.arguments), live)
            else:
                check_text(response.choices[0].message.content or "", live)

    for stream in (False, True):
        for use_tools in (False, True):
            print(f"Checking responses stream={stream} tools={use_tools}", flush=True)
            kwargs = {"model": MODEL, "input": prompt(use_tools, live), "stream": stream}
            if live:
                kwargs.update(max_output_tokens=512 if use_tools else 64, temperature=0)
            if use_tools:
                kwargs["tools"] = [{"type": "function", "name": "get_weather", "parameters": SCHEMA, "strict": False}]
            response = client.responses.create(**kwargs)
            if stream:
                events = list(response)
                completed = next(event for event in events if event.type == "response.completed")
                output = completed.response.output
            else:
                output = response.output
            if use_tools:
                call = next(item for item in output if item.type == "function_call")
                assert call.name == "get_weather"
                check_city(json.loads(call.arguments), live)
            else:
                check_text(next(item for item in output if item.type == "message").content[0].text, live)
    print("Checking responses stream aggregator", flush=True)
    with client.responses.stream(model=MODEL, input=prompt(True, live), tools=[{"type": "function", "name": "get_weather", "parameters": SCHEMA, "strict": False}], **({"max_output_tokens": 512, "temperature": 0} if live else {})) as stream:
        output = stream.get_final_response().output
        call = next(item for item in output if item.type == "function_call")
        assert call.name == "get_weather"
        check_city(json.loads(call.arguments), live)


def test_anthropic(base, live=False):
    client = Anthropic(api_key=KEY, base_url=base, timeout=120 if live else 10, max_retries=0)
    for stream in (False, True):
        for use_tools in (False, True):
            print(f"Checking anthropic stream={stream} tools={use_tools}", flush=True)
            kwargs = {"model": MODEL, "max_tokens": (512 if use_tools else 64) if live else 100, "messages": [{"role": "user", "content": prompt(use_tools, live)}], "stream": stream}
            if use_tools:
                kwargs["tools"] = [{"name": "get_weather", "input_schema": SCHEMA}]
            response = client.messages.create(**kwargs)
            if stream:
                events = list(response)
                assert events[-1].type == "message_stop"
                if use_tools:
                    block = next(e.content_block for e in events if e.type == "content_block_start" and e.content_block.type == "tool_use")
                    assert block.type == "tool_use" and block.name == "get_weather"
                    delta = next(e.delta for e in events if e.type == "content_block_delta" and e.delta.type == "input_json_delta")
                    check_city(json.loads(delta.partial_json), live)
                else:
                    check_text("".join(e.delta.text for e in events if e.type == "content_block_delta" and e.delta.type == "text_delta"), live)
            elif use_tools:
                block = next(item for item in response.content if item.type == "tool_use")
                assert block.name == "get_weather"
                check_city(block.input, live)
            else:
                check_text(next(item for item in response.content if item.type == "text").text, live)
    print("Checking anthropic stream aggregator", flush=True)
    with client.messages.stream(model=MODEL, max_tokens=512 if live else 100, messages=[{"role": "user", "content": prompt(True, live)}], tools=[{"name": "get_weather", "input_schema": SCHEMA}]) as stream:
        block = next(item for item in stream.get_final_message().content if item.type == "tool_use")
        assert block.name == "get_weather"
        check_city(block.input, live)


def test_gemini(base, live=False):
    client = genai.Client(api_key=KEY, http_options=types.HttpOptions(base_url=base, api_version="v1beta"))
    tool = types.Tool(function_declarations=[types.FunctionDeclaration(name="get_weather", parameters=SCHEMA)])
    for stream in (False, True):
        for use_tools in (False, True):
            print(f"Checking gemini stream={stream} tools={use_tools}", flush=True)
            kwargs = {"model": MODEL, "contents": prompt(use_tools, live)}
            if live or use_tools:
                kwargs["config"] = types.GenerateContentConfig(tools=[tool] if use_tools else None, max_output_tokens=(512 if use_tools else 64) if live else None, temperature=0 if live else None)
            response = client.models.generate_content_stream(**kwargs) if stream else [client.models.generate_content(**kwargs)]
            parts = [part for chunk in response for candidate in (chunk.candidates or []) if candidate.content for part in (candidate.content.parts or [])]
            if use_tools:
                call = next(part.function_call for part in parts if part.function_call)
                assert call.name == "get_weather"
                check_city(call.args, live)
            else:
                check_text("".join(part.text or "" for part in parts), live)
    client.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--live", action="store_true", help="use the real CNB upstream")
    parser.add_argument("--session-prompt", action="store_true", help="read the Cookie header without echoing it")
    parser.add_argument("--surface", choices=["all", "openai", "anthropic", "gemini"], default="all")
    parser.add_argument("--mock-live-inputs", action="store_true", help="exercise live request parameters against the mock")
    args = parser.parse_args()
    if args.session_prompt and not args.live:
        parser.error("--session-prompt requires --live")
    if args.mock_live_inputs and args.live:
        parser.error("--mock-live-inputs cannot be combined with --live")
    binary_name = "cnb-npc2api.exe" if os.name == "nt" else "cnb-npc2api"
    default_binary = str(Path("target") / ("release" if args.live else "debug") / binary_name)
    binary = Path(os.environ.get("CNB_NPC2API_BINARY") or os.environ.get("CNB_FORGE_BINARY") or default_binary).resolve()
    if not binary.is_file():
        raise FileNotFoundError(binary)
    mock = None
    if not args.live:
        mock = ThreadingHTTPServer(("127.0.0.1", 0), MockCnb)
        thread = threading.Thread(target=mock.serve_forever, daemon=True)
        thread.start()
    listen = port()
    base = f"http://127.0.0.1:{listen}"
    with tempfile.TemporaryDirectory(prefix="cnb-npc2api-sdk-") as tmp:
        config = Path(tmp) / "config.yaml"
        config.write_text(
            f"server:\n  host: 127.0.0.1\n  port: {listen}\n"
            f"client_authentication:\n  enabled: true\n  allowed_keys: [{KEY}]\n"
            f"upstreams:\n  - name: cnb\n    type: cnb\n    base_url: {'https://cnb.cool' if args.live else f'http://127.0.0.1:{mock.server_port}'}\n"
            "    session_cookie: '${CNB_SESSION_COOKIE}'\n"
            f"    models: [{MODEL}]\n    native_fc: false\n    pool_min: 1\n    pool_max: 2\n",
            encoding="utf-8",
        )
        env = os.environ.copy()
        if args.session_prompt:
            env["CNB_SESSION_COOKIE"] = getpass.getpass("CNB Cookie: ")
        elif not args.live:
            env.pop("CNB_SESSION_COOKIE", None)
        process = subprocess.Popen([str(binary), "--config", str(config)], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, env=env,
                                   creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
        try:
            wait_ready(base, process)
            live_inputs = args.live or args.mock_live_inputs
            if args.surface in ("all", "openai"):
                test_openai(base, live_inputs)
            if args.surface in ("all", "anthropic"):
                test_anthropic(base, live_inputs)
            if args.surface in ("all", "gemini"):
                test_gemini(base, live_inputs)
            summary = {"all": "16 client scenarios and 2 stream aggregators", "openai": "8 client scenarios and 1 stream aggregator", "anthropic": "4 client scenarios and 1 stream aggregator", "gemini": "4 client scenarios"}[args.surface]
            print(f"{'Live' if args.live else 'Mock'} SDK smoke: {summary} passed")
            if mock:
                print(f"Mock CNB requests: {len(MockCnb.calls)}")
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            if mock:
                mock.shutdown()
                mock.server_close()


if __name__ == "__main__":
    main()
