"""Probe the upstream protocol with optional session cookies, without displaying credentials."""

import http.cookiejar
import http.cookies
import argparse
import getpass
import json
import os
import re
import urllib.error
import urllib.request

BASE = "https://cnb.cool"
USER_AGENT = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--identity-only", action="store_true")
    parser.add_argument("--session-prompt", action="store_true", help="read the Cookie header without echoing it")
    args = parser.parse_args()
    jar = http.cookiejar.CookieJar()
    supplied = http.cookies.SimpleCookie()
    raw_cookie = getpass.getpass("CNB Cookie: ") if args.session_prompt else os.environ.get("CNB_SESSION_COOKIE", "")
    try:
        supplied.load(raw_cookie)
    except http.cookies.CookieError:
        raise RuntimeError("invalid session cookie format") from None
    if raw_cookie and not supplied:
        raise RuntimeError("invalid session cookie format")
    for name, item in supplied.items():
        jar.set_cookie(http.cookiejar.Cookie(
            version=0, name=name, value=item.value, port=None, port_specified=False,
            domain="cnb.cool", domain_specified=False, domain_initial_dot=False,
            path="/", path_specified=True, secure=True, expires=None, discard=True,
            comment=None, comment_url=None, rest={}, rfc2109=False,
        ))
    client = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(jar))
    request = urllib.request.Request(BASE + "/", headers={"User-Agent": USER_AGENT, "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"})
    with client.open(request, timeout=30) as response:
        html = response.read(8 * 1024 * 1024).decode("utf-8", errors="replace")
    match = re.search(r"window\.csrftoken\s*=\s*[\"']([0-9a-fA-F]{32,64})[\"']", html)
    key = next((cookie.value for cookie in jar if cookie.name == "csrfkey"), "")
    print(f"Homepage credential fields present: cookie={bool(key)}, token={bool(match)}")
    if supplied:
        retained = all(any(cookie.name == name and cookie.value == item.value for cookie in jar) for name, item in supplied.items())
        print(f"Session fields unchanged after homepage: {retained}")
    if not key or not match:
        raise RuntimeError("homepage credential fields missing")
    token = match.group(1)
    request = urllib.request.Request(BASE + "/user", headers={"User-Agent": USER_AGENT, "Csrftoken": token, "Accept": "application/json"})
    jar.add_cookie_header(request)
    sent = http.cookies.SimpleCookie()
    sent.load(request.get_header("Cookie", ""))
    if supplied:
        print(f"Session fields sent to identity endpoint: {all(name in sent and sent[name].value == item.value for name, item in supplied.items())}")
    authenticated = False
    try:
        with client.open(request, timeout=30) as response:
            identity = json.load(response)
            identity = identity.get("result", identity) if isinstance(identity, dict) else identity
            authenticated = isinstance(identity, dict) and bool(identity.get("username") or identity.get("userName"))
            print(f"Identity endpoint status: {response.status}; recognized user: {authenticated}")
    except urllib.error.HTTPError as error:
        try:
            error_data = json.loads(error.read(4096))
            message = str(error_data.get("errmsg", error_data.get("message", ""))).lower()
            code = error_data.get("errcode")
            reason = "NOT_LOGIN" if "not_login" in message else "EXPIRED" if "expir" in message or "过期" in message else "CSRF" if "csrf" in message else "other"
            print(f"Identity endpoint status: {error.code}; error code: {code if isinstance(code, int) else 'unknown'}; reason: {reason}")
        except (ValueError, AttributeError):
            print(f"Identity endpoint status: {error.code}; non-JSON error response")
    except (ValueError, UnicodeError):
        print("Identity endpoint returned a non-JSON response")
    if args.identity_only:
        if not authenticated:
            raise SystemExit(1)
        return
    body = json.dumps({"model": "deepseek-v4-flash", "stream": True, "messages": [{"role": "user", "content": "What is 2+2? Answer briefly."}], "maxTokens": 64}).encode()
    headers = {"Content-Type": "application/json", "Accept": "text/event-stream, application/json, text/plain, */*",
               "User-Agent": USER_AGENT, "Origin": BASE, "Referer": BASE + "/", "Csrftoken": token}
    request = urllib.request.Request(BASE + "/ai/chat/completions", data=body, headers=headers)
    try:
        with client.open(request, timeout=60) as response:
            print(f"Direct upstream status: {response.status}; content type: {response.headers.get_content_type()}")
    except urllib.error.HTTPError as error:
        raw = error.read(4096).decode("utf-8", errors="replace")
        try:
            data = json.loads(raw)
            message = str(data.get("errmsg", data.get("message", "")))
            reason = "NOT_LOGIN" if "NOT_LOGIN" in message else "CSRF" if "csrf" in message.lower() else "other"
            code = data.get("errcode")
            print(f"Direct upstream status: {error.code}; error code: {code if isinstance(code, int) else 'unknown'}; reason: {reason}")
        except (ValueError, AttributeError):
            print(f"Direct upstream status: {error.code}; non-JSON error response")
        raise SystemExit(1)


if __name__ == "__main__":
    main()
