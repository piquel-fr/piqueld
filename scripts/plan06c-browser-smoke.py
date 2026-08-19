#!/usr/bin/env python3
"""Exercise the production Plan 06C dashboard without external test packages."""

from __future__ import annotations

import json
import mimetypes
import os
from pathlib import Path
import shutil
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse


ROOT = Path(__file__).resolve().parent.parent
DIST = Path(os.environ.get("PIQUELD_UI_DIST", ROOT / "target/piqueld-ui-dist"))
APP_IDS = {"alpha": "app-alpha-01", "beta": "app-beta-01"}


def envelope(data: object) -> bytes:
    return json.dumps({"data": data}, separators=(",", ":")).encode()


def application(name: str) -> dict[str, object]:
    return {
        "application": {
            "id": APP_IDS[name],
            "api_version": "piqueld.dev/v1alpha1",
            "kind": "Application",
            "metadata": {"name": name},
            "spec": {
                "services": [
                    {
                        "name": "web",
                        "source": {
                            "type": "image",
                            "image": f"ghcr.io/example/{name}:1.0.0",
                        },
                        "replicas": 1,
                        "environment": {},
                        "command": [],
                        "arguments": [],
                        "mounts": [],
                        "healthcheck": None,
                        "resources": None,
                    }
                ],
                "volumes": [],
            },
        },
        "generation": 1,
        "spec_hash": "sha256:" + "a" * 64,
        "delete_intent": False,
        "created_at_ms": 1,
        "updated_at_ms": 1,
    }


def status(name: str) -> dict[str, object]:
    return {
        "application_id": APP_IDS[name],
        "state": "converged",
        "observed_generation": 1,
        "message": None,
        "updated_at_ms": 1,
    }


def detail(name: str) -> dict[str, object]:
    return {
        "application": application(name),
        "status": status(name),
        "observed": {
            "services": [
                {
                    "name": "web",
                    "image": f"ghcr.io/example/{name}:1.0.0",
                    "desired_replicas": 1,
                    "observed_replicas": 1,
                    "healthy_replicas": 1,
                    "convergence": "converged",
                    "diagnostics": [],
                }
            ],
            "network_count": 1,
            "volume_count": 0,
        },
        "latest_operation": None,
        "diagnostics": [],
    }


DRIVER = r"""
<script>
const scenario = new URL(location.href).searchParams.get("smoke");
const sleep = milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds));
async function waitFor(test, label, timeout = 5000) {
  const deadline = performance.now() + timeout;
  while (performance.now() < deadline) {
    const value = test();
    if (value) return value;
    await sleep(25);
  }
  throw new Error(`timed out waiting for ${label}`);
}
function text(value) {
  return document.body.innerText.includes(value);
}
function button(label) {
  return [...document.querySelectorAll("button")].find(candidate => candidate.textContent.trim() === label);
}
async function refresh() {
  const control = await waitFor(
    () => document.querySelector(".header-actions button:not([disabled])"),
    "enabled refresh control",
  );
  control.click();
}
function card(name) {
  return [...document.querySelectorAll(".application-card")].find(candidate => candidate.querySelector("h3")?.textContent.trim() === name);
}
async function mode(value) {
  const response = await fetch(`/__smoke/mode?value=${value}`, {cache: "no-store"});
  if (!response.ok) throw new Error(`could not select ${value} API mode`);
}
async function desktop() {
  await waitFor(() => text("No applications"), "empty state");
  if (document.querySelector("form, input, textarea, select")) throw new Error("mutation form present");
  const mutation = /^(new|create|apply|delete|replace|reconcile|set secret)/i;
  if ([...document.querySelectorAll("button")].some(item => mutation.test(item.textContent.trim()))) {
    throw new Error("mutation control present");
  }

  await mode("healthy");
  await refresh();
  await waitFor(() => card("alpha") && card("beta"), "populated application list");

  card("alpha").querySelector("button").click();
  await sleep(20);
  const beta = card("beta").querySelector("button");
  beta.focus();
  if (document.activeElement !== beta) throw new Error("detail control is not keyboard focusable");
  beta.click();
  await waitFor(
    () => document.querySelector(".detail-title-row h3")?.textContent.trim() === "beta",
    "latest selected application detail",
  );
  if (!card("beta").classList.contains("selected")) throw new Error("selected card and detail disagree");

  await mode("error");
  await refresh();
  await waitFor(() => text("Showing the last successful view"), "stale state");
  if (!card("alpha") || !card("beta")) throw new Error("stale state discarded prior data");

  await mode("disconnect");
  await refresh();
  await waitFor(() => text("Daemon unreachable"), "unreachable state");
  if (!button("Try again")) throw new Error("unreachable state has no retry control");

  await mode("healthy");
  await refresh();
  await waitFor(() => text("Daemon: Reachable") && !text("Showing the last successful view"), "recovery");
}
async function narrow() {
  await waitFor(() => card("alpha") && card("beta"), "narrow application list");
  if (document.documentElement.scrollWidth > window.innerWidth) throw new Error("narrow layout overflows");
  if (getComputedStyle(document.querySelector(".dashboard-grid")).gridTemplateColumns.split(" ").length !== 1) {
    throw new Error("narrow layout did not collapse to one column");
  }
}
addEventListener("TrunkApplicationStarted", async () => {
  try {
    await (scenario === "narrow" ? narrow() : desktop());
    document.body.dataset.browserSmoke = "pass";
  } catch (error) {
    document.body.dataset.browserSmoke = "fail";
    const output = document.createElement("pre");
    output.id = "browser-smoke-error";
    output.textContent = error?.stack ?? String(error);
    document.body.append(output);
  }
});
</script>
"""


class State:
    def __init__(self) -> None:
        self.mode = "empty"
        self.lock = threading.Lock()


STATE = State()


class Handler(BaseHTTPRequestHandler):
    def log_message(self, _format: str, *_args: object) -> None:
        return

    def send_bytes(self, status_code: int, body: bytes, content_type: str) -> None:
        self.send_response(status_code)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def json(self, value: object, status_code: int = 200) -> None:
        self.send_bytes(status_code, envelope(value), "application/json")

    def do_GET(self) -> None:  # noqa: N802 - stdlib handler API
        parsed = urlparse(self.path)
        if parsed.path == "/__smoke/mode":
            value = parse_qs(parsed.query).get("value", [""])[0]
            if value not in {"empty", "healthy", "error", "disconnect"}:
                self.send_bytes(400, b"invalid mode", "text/plain")
                return
            with STATE.lock:
                STATE.mode = value
            self.send_bytes(204, b"", "text/plain")
            return

        with STATE.lock:
            mode = STATE.mode
        if parsed.path == "/api/v1/system/status":
            if mode == "disconnect":
                self.connection.close()
                return
            self.json(
                {
                    "status": "running",
                    "api_version": "v1",
                    "daemon_version": "0.1.0",
                    "instance_id": "instance-browser-smoke",
                }
            )
            return
        if parsed.path == "/api/v1/applications":
            if mode == "error":
                self.send_bytes(
                    503,
                    json.dumps(
                        {
                            "code": "storage_unavailable",
                            "message": "control-plane storage is unavailable",
                            "request_id": "browser-smoke",
                        }
                    ).encode(),
                    "application/json",
                )
                return
            items = [] if mode == "empty" else [application("alpha"), application("beta")]
            self.json({"items": items, "next_cursor": None})
            return
        for name, application_id in APP_IDS.items():
            if parsed.path == f"/api/v1/applications/{application_id}/status":
                self.json(status(name))
                return
            if parsed.path == f"/api/v1/applications/{application_id}/detail":
                if name == "alpha":
                    time.sleep(0.35)
                self.json(detail(name))
                return

        if parsed.path == "/":
            source = (DIST / "index.html").read_text()
            body = source.replace("</body>", f"{DRIVER}</body>").encode()
            self.send_bytes(200, body, "text/html; charset=utf-8")
            return
        relative = parsed.path.removeprefix("/")
        candidate = (DIST / relative).resolve()
        if DIST.resolve() not in candidate.parents or not candidate.is_file():
            self.send_bytes(404, b"not found", "text/plain")
            return
        content_type = mimetypes.guess_type(candidate)[0] or "application/octet-stream"
        if candidate.suffix == ".wasm":
            content_type = "application/wasm"
        self.send_bytes(200, candidate.read_bytes(), content_type)


def browser() -> str:
    configured = os.environ.get("PIQUELD_BROWSER")
    if configured:
        return configured
    for candidate in ("chromium", "chromium-browser", "google-chrome"):
        found = shutil.which(candidate)
        if found:
            return found
    raise RuntimeError("no Chromium-compatible browser found; set PIQUELD_BROWSER")


def run_scenario(executable: str, port: int, scenario: str, width: int, height: int) -> None:
    with STATE.lock:
        STATE.mode = "healthy" if scenario == "narrow" else "empty"
    process = subprocess.run(
        [
            executable,
            "--headless",
            "--no-sandbox",
            "--disable-gpu",
            "--disable-dev-shm-usage",
            f"--window-size={width},{height}",
            "--virtual-time-budget=10000",
            "--dump-dom",
            f"http://127.0.0.1:{port}/?smoke={scenario}",
        ],
        check=False,
        capture_output=True,
        text=True,
        timeout=30,
    )
    if process.returncode != 0 or 'data-browser-smoke="pass"' not in process.stdout:
        print(process.stderr[-8000:], file=sys.stderr)
        print(process.stdout[-20000:], file=sys.stderr)
        raise RuntimeError(f"{scenario} browser smoke failed")


def main() -> int:
    if not (DIST / "index.html").is_file():
        print(f"UI bundle missing at {DIST}; run just ui-build first", file=sys.stderr)
        return 2
    try:
        executable = browser()
    except RuntimeError as error:
        print(error, file=sys.stderr)
        return 2
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        port = server.server_address[1]
        run_scenario(executable, port, "desktop", 1280, 800)
        run_scenario(executable, port, "narrow", 390, 844)
    finally:
        server.shutdown()
        server.server_close()
        thread.join()
    print("Plan 06C desktop and narrow browser smoke passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
