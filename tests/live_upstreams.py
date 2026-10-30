"""Exercise one external upstream through the gateway with a locally supplied API key."""

import argparse
import getpass
import json
import os
import secrets
import subprocess
import sys
import tempfile
import urllib.error
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from threading import Thread

import sdk_smoke as sdk

PROVIDERS = {
    "openai": ("openai_compat", "https://api.openai.com/v1", "OPENAI_API_KEY"),
    "anthropic": ("anthropic", "https://api.anthropic.com", "ANTHROPIC_API_KEY"),
    "gemini": ("gemini", "https://generativelanguage.googleapis.com", "GEMINI_API_KEY"),
}
PIXEL = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+nmWQAAAAASUVORK5CYII="


class MockProvider(BaseHTTPRequestHandler):
    provider = ""
    calls = []

    def log_message(self, *args):
        pass

    def do_POST(self):
        expected = {"openai": ("Authorization", "Bearer mock-upstream-key"), "anthropic": ("x-api-key", "mock-upstream-key"), "gemini": ("x-goog-api-key", "mock-upstream-key")}[self.provider]
        if self.headers.get(expected[0]) != expected[1]:
            self.send_error(401)
            return
        length = int(self.headers.get("Content-Length", "0"))
        request = json.loads(self.rfile.read(length))
        self.calls.append((self.path, request))
        builtin = any("web_search" in str(tool.get("type", "")) or "googleSearch" in tool for tool in request.get("tools", []))
        if builtin:
            if self.provider == "openai":
                reply = {"id": "resp_mock", "object": "response", "output": [{"type": "web_search_call", "id": "search_mock", "status": "completed"}]}
            elif self.provider == "anthropic":
                reply = {"id": "msg_mock", "type": "message", "role": "assistant", "content": [{"type": "server_tool_use", "id": "search_mock", "name": "web_search", "input": {"query": "UTC date"}}]}
            else:
                reply = {"candidates": [{"content": {"role": "model", "parts": [{"text": "Mock result"}]}, "groundingMetadata": {"webSearchQueries": ["UTC date"]}}]}
            data = json.dumps(reply).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return
        native = bool(request.get("tools"))
        streamed = bool(request.get("stream")) or ":streamGenerateContent" in self.path
        call = {"name": "get_weather", "args": {"city": "Tokyo"}}
        if self.provider == "openai":
            message = {"role": "assistant", "content": None if native else "hello from mock"}
            if native:
                message["tool_calls"] = [{"id": "call_test", "type": "function", "function": {"name": call["name"], "arguments": json.dumps(call["args"])}}]
            reply = {"id": "upstream", "choices": [{"message": message, "finish_reason": "tool_calls" if native else "stop"}]}
            delta = {"tool_calls": [{"index": 0, "id": "call_test", "type": "function", "function": {"name": call["name"], "arguments": json.dumps(call["args"])}}]} if native else {"content": "hello from mock"}
            events = [(None, {"choices": [{"delta": delta, "finish_reason": None}]}), (None, {"choices": [{"delta": {}, "finish_reason": "tool_calls" if native else "stop"}]})]
        elif self.provider == "anthropic":
            block = {"type": "tool_use", "id": "call_test", "name": call["name"], "input": call["args"]} if native else {"type": "text", "text": "hello from mock"}
            reply = {"id": "upstream", "content": [block], "stop_reason": "tool_use" if native else "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1}}
            start = {"type": "tool_use", "id": "call_test", "name": call["name"], "input": {}} if native else {"type": "text", "text": ""}
            delta = {"type": "input_json_delta", "partial_json": json.dumps(call["args"])} if native else {"type": "text_delta", "text": "hello from mock"}
            events = [("message_start", {"type": "message_start", "message": {"usage": {"input_tokens": 1}}}), ("content_block_start", {"type": "content_block_start", "index": 0, "content_block": start}), ("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": delta}), ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "tool_use" if native else "end_turn"}, "usage": {"output_tokens": 1}}), ("message_stop", {"type": "message_stop"})]
        else:
            part = {"functionCall": call} if native else {"text": "hello from mock"}
            reply = {"candidates": [{"content": {"role": "model", "parts": [part]}, "finishReason": "STOP"}]}
            events = [(None, reply)]
        if streamed:
            payload = "".join((f"event: {event}\n" if event else "") + "data: " + json.dumps(value) + "\n\n" for event, value in events)
            if self.provider == "openai":
                payload += "data: [DONE]\n\n"
            content_type = "text/event-stream"
        else:
            payload = json.dumps(reply)
            content_type = "application/json"
        data = payload.encode()
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


def vision_check(base, key, model):
    body = {"model": model, "max_tokens": 64, "messages": [{"role": "user", "content": [{"type": "text", "text": "Briefly describe the image."}, {"type": "image_url", "image_url": {"url": "data:image/png;base64," + PIXEL}}]}]}
    request = urllib.request.Request(base + "/v1/chat/completions", data=json.dumps(body).encode(), headers={"Authorization": "Bearer " + key, "Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=120) as response:
        result = json.load(response)
        if not result["choices"][0]["message"].get("content"):
            raise RuntimeError("vision response contained no text")
    print("Vision input: completed", flush=True)


def native_tool_check(base, key, model, provider):
    prompt = "Use web search to look up the current UTC date and report a source."
    if provider == "openai":
        path = "/v1/responses"
        body = {"model": model, "input": prompt, "tools": [{"type": "web_search"}], "tool_choice": "required", "store": False, "max_output_tokens": 512}
    elif provider == "anthropic":
        path = "/v1/messages"
        body = {"model": model, "max_tokens": 512, "messages": [{"role": "user", "content": prompt}], "tools": [{"type": "web_search_20250305", "name": "web_search"}]}
    else:
        path = f"/v1beta/models/{urllib.parse.quote(model, safe='')}:generateContent"
        body = {"contents": [{"role": "user", "parts": [{"text": prompt}]}], "tools": [{"googleSearch": {}}], "generationConfig": {"maxOutputTokens": 512}}
    request = urllib.request.Request(base + path, data=json.dumps(body).encode(), headers={"Authorization": "Bearer " + key, "Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=120) as response:
        result = json.load(response)
    if provider == "openai":
        used = any(item.get("type") == "web_search_call" for item in result.get("output", []))
    elif provider == "anthropic":
        used = any(item.get("type") in ("server_tool_use", "web_search_tool_result") for item in result.get("content", []))
    else:
        used = any(candidate.get("groundingMetadata") or candidate.get("grounding_metadata") for candidate in result.get("candidates", []))
    if not used:
        raise RuntimeError("the upstream did not report using its native search tool")
    print("Native search tool: reported by upstream", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--provider", required=True, choices=PROVIDERS)
    parser.add_argument("--model", required=True, help="model name supported by the selected upstream")
    parser.add_argument("--base-url", help="upstream base URL; defaults to the provider's official API")
    parser.add_argument("--key-prompt", action="store_true", help="read the provider API key without echoing it")
    parser.add_argument("--vision", action="store_true", help="send one additional tiny image request")
    parser.add_argument("--native-tools", action="store_true", help="make an additional native web-search request")
    parser.add_argument("--mock", action="store_true", help="test the harness against an isolated local fixture")
    args = parser.parse_args()
    kind, default_url, env_name = PROVIDERS[args.provider]
    mock = None
    if args.mock:
        if args.key_prompt or args.base_url:
            parser.error("--mock cannot be combined with --key-prompt or --base-url")
        MockProvider.provider = args.provider
        MockProvider.calls = []
        mock = ThreadingHTTPServer(("127.0.0.1", 0), MockProvider)
        Thread(target=mock.serve_forever, daemon=True).start()
        base_url = f"http://127.0.0.1:{mock.server_port}" + ("/v1" if args.provider == "openai" else "")
        upstream_key = "mock-upstream-key"
    else:
        base_url = args.base_url or default_url
        upstream_key = getpass.getpass("Upstream API key: ") if args.key_prompt else os.environ.get(env_name, "")
        if not upstream_key:
            parser.error(f"provide {env_name} locally or use --key-prompt")
    parsed = urllib.parse.urlsplit(base_url)
    if parsed.username or parsed.password or parsed.query or parsed.fragment or not (parsed.scheme == "https" or (parsed.scheme == "http" and parsed.hostname in ("127.0.0.1", "localhost", "::1"))):
        parser.error("base URL must use HTTPS, except for localhost")
    binary_name = "cnb-npc2api.exe" if os.name == "nt" else "cnb-npc2api"
    binary = Path(os.environ.get("CNB_NPC2API_BINARY") or os.environ.get("CNB_FORGE_BINARY") or str(Path("target/release") / binary_name)).resolve()
    if not binary.is_file():
        raise FileNotFoundError(binary)
    port = sdk.port()
    base = f"http://127.0.0.1:{port}"
    local_key = secrets.token_urlsafe(32)
    sdk.KEY, sdk.MODEL = local_key, args.model
    config = {"server": {"host": "127.0.0.1", "port": port}, "client_authentication": {"enabled": True, "allowed_keys": [local_key]}, "upstreams": [{"name": args.provider, "type": kind, "base_url": base_url, "api_key": "${LIVE_UPSTREAM_API_KEY}", "models": [args.model], "native_fc": True}]}
    try:
        with tempfile.TemporaryDirectory(prefix="cnb-npc2api-upstream-") as tmp:
            config_path = Path(tmp) / "config.yaml"
            config_path.write_text(json.dumps(config), encoding="utf-8")
            env = os.environ.copy()
            env["LIVE_UPSTREAM_API_KEY"] = upstream_key
            process = subprocess.Popen([str(binary), "--config", str(config_path)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env, creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
            try:
                sdk.wait_ready(base, process)
                {"openai": sdk.test_openai, "anthropic": sdk.test_anthropic, "gemini": sdk.test_gemini}[args.provider](base, True)
                if args.vision:
                    vision_check(base, local_key, args.model)
                    if mock and not any(PIXEL in json.dumps(request) for _, request in MockProvider.calls):
                        raise RuntimeError("mock upstream did not receive the image")
                if args.native_tools:
                    native_tool_check(base, local_key, args.model, args.provider)
                print(f"{'Mock' if args.mock else 'Live'} {args.provider} upstream checks passed", flush=True)
            finally:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
    finally:
        if mock:
            mock.shutdown()
            mock.server_close()


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"Upstream check failed: {type(error).__name__}", file=sys.stderr)
        raise SystemExit(1) from None
