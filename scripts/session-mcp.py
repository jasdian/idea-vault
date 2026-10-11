#!/usr/bin/env python3
"""Drive a running idea-vault over its inbound MCP endpoint (docs/13, ADR-0024) from a shell.

Meant for agent sessions (Claude Code on the web, CI, a terminal) that cannot or should not drive
the HTMX UI. Standard library only. One MCP session per invocation:

    scripts/session-mcp.py tools                       # the tool catalog: name + one-line summary
    scripts/session-mcp.py call list_ideas
    scripts/session-mcp.py call create_idea '{"title": "…", "body": "…"}'
    scripts/session-mcp.py call chat '{"slug": "…", "message": "…"}'

A long-running tool (chat, run_skill, run_swarm, run_workflow, store_idea, build_plan, make_skill)
answers a plain call after a short bounded wait with a "still running" note (ADR-0028). This client
then repeats the identical call, which reattaches to the same job and never starts a second one
(ADR-0033), until the result lands or --wait seconds pass.

Endpoint and token come from IDEA_VAULT_SESSION_URL (default http://127.0.0.1:3000/api/mcp) and
IDEA_VAULT_MCP_TOKEN, which scripts/session-up.sh writes. The token file
target/session/mcp-token is the fallback.
"""
import argparse
import json
import os
import pathlib
import sys
import time
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent
TOKEN_FILE = ROOT / "target" / "session" / "mcp-token"
STILL_RUNNING = "is still running for idea"
PROTOCOL_VERSION = "2025-06-18"


def token() -> str:
    env = os.environ.get("IDEA_VAULT_MCP_TOKEN", "").strip()
    if env:
        return env
    if TOKEN_FILE.is_file():
        return TOKEN_FILE.read_text().strip()
    sys.exit("no MCP token: run scripts/session-up.sh first (or set IDEA_VAULT_MCP_TOKEN)")


def parse_body(content_type: str, raw: bytes) -> dict | None:
    """A Streamable-HTTP reply is either one JSON object or an SSE stream carrying it."""
    text = raw.decode("utf-8", "replace")
    if not text.strip():
        return None
    if "text/event-stream" in content_type:
        messages = [
            json.loads(line[len("data:"):].strip())
            for line in text.splitlines()
            if line.startswith("data:") and line[len("data:"):].strip()
        ]
        replies = [m for m in messages if "result" in m or "error" in m]
        return replies[-1] if replies else None
    return json.loads(text)


class Session:
    def __init__(self, url: str, bearer: str):
        self.url = url
        self.bearer = bearer
        self.session_id: str | None = None
        self.next_id = 0

    def _post(self, payload: dict, timeout: float) -> dict | None:
        headers = {
            "Authorization": f"Bearer {self.bearer}",
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
            "MCP-Protocol-Version": PROTOCOL_VERSION,
        }
        if self.session_id:
            headers["Mcp-Session-Id"] = self.session_id
        req = urllib.request.Request(self.url, json.dumps(payload).encode(), headers, method="POST")
        # The endpoint is loopback; an agent proxy in HTTPS_PROXY/HTTP_PROXY must not intercept it.
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        try:
            with opener.open(req, timeout=timeout) as resp:
                self.session_id = resp.headers.get("Mcp-Session-Id") or self.session_id
                return parse_body(resp.headers.get("Content-Type", ""), resp.read())
        except urllib.error.HTTPError as e:
            sys.exit(f"HTTP {e.code} from {self.url}: {e.read().decode('utf-8', 'replace')[:500]}")
        except urllib.error.URLError as e:
            sys.exit(f"cannot reach {self.url} ({e.reason}): is idea-vault up? run scripts/session-up.sh")

    def request(self, method: str, params: dict, timeout: float = 60) -> dict:
        self.next_id += 1
        reply = self._post({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params}, timeout)
        if reply is None:
            sys.exit(f"{method}: empty reply")
        if "error" in reply:
            sys.exit(f"{method}: {reply['error'].get('message', reply['error'])}")
        return reply["result"]

    def open(self) -> "Session":
        self.request("initialize", {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "idea-vault-session-mcp", "version": "1"},
        })
        self._post({"jsonrpc": "2.0", "method": "notifications/initialized"}, 10)
        return self


def text_of(result: dict) -> str:
    return "\n".join(c.get("text", "") for c in result.get("content", []) if c.get("type") == "text")


def call(session: Session, name: str, args: dict, wait: float, poll: float) -> int:
    deadline = time.monotonic() + wait
    while True:
        result = session.request("tools/call", {"name": name, "arguments": args}, timeout=120)
        out = text_of(result)
        if STILL_RUNNING not in out:
            print(out)
            return 1 if result.get("isError") else 0
        if time.monotonic() >= deadline:
            print(out)
            print(f"(gave up waiting after {wait:.0f}s; repeat the same call to collect the result)", file=sys.stderr)
            return 2
        print(f"… {name} still running", file=sys.stderr, flush=True)
        time.sleep(poll)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--url", default=os.environ.get("IDEA_VAULT_SESSION_URL", "http://127.0.0.1:3000/api/mcp"))
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("tools", help="list the tool catalog")
    c = sub.add_parser("call", help="call one tool, waiting out a long-running job")
    c.add_argument("tool")
    c.add_argument("args", nargs="?", default="{}", help="JSON object of arguments, or @file.json")
    c.add_argument("--wait", type=float, default=1800, help="seconds to keep collecting (default 1800)")
    c.add_argument("--poll", type=float, default=5, help="seconds between collection retries")
    ns = ap.parse_args()

    session = Session(ns.url, token()).open()
    if ns.cmd == "tools":
        for t in session.request("tools/list", {})["tools"]:
            print(f"{t['name']:<16} {(t.get('description') or '').split('.')[0]}")
        return 0
    raw = pathlib.Path(ns.args[1:]).read_text() if ns.args.startswith("@") else ns.args
    return call(session, ns.tool, json.loads(raw), ns.wait, ns.poll)


if __name__ == "__main__":
    sys.exit(main())
