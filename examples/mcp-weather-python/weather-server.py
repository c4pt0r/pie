#!/usr/bin/env python3
# MCP server that turns a real-world data source (SF weather) into pie trigger events.
# It is the heartbeat demo (../mcp-notify-python) with the synthetic counter swapped for a
# live poll of wttr.in. Speaks JSON-RPC 2.0 over stdio, line-delimited (one msg per line).
#
# Division of labour — this is the whole point of the example:
#   * The SERVER does the IO it is uniquely able to do: reach the network, parse the API,
#     and collapse the reading into ONE human-readable sentence (`_meta.pie_summary`).
#   * pie's HARNESS does the reasoning: a session trigger rule decides what that sentence
#     means and what to do about it (log it, alert on rain, etc). The server ships no
#     policy and no agent logic — just the sentence.
#
# Requests handled:
#   - initialize   -> InitializeResult (protocolVersion 2025-03-26)
#   - tools/list   -> empty tool list (this server pushes; it exposes no callable tools)
#   - tools/call   -> RPC error -32601
#   - anything else-> RPC error -32601 "method not found"
#
# Notifications emitted (no `id`, server-pushed):
#   - notifications/pie/weather/observation  on each new observation, carrying
#     `_meta.pie_dedup_key` (keyed on the observation timestamp, so re-polling the same
#     reading does NOT re-fire) and `_meta.pie_summary` (the one-line weather sentence).
#
# Config via env (all optional):
#   PIE_WEATHER_LOCATION       location query for wttr.in   (default "San Francisco")
#   PIE_WEATHER_INTERVAL_SECS  poll interval in seconds      (default 600)
#
# Wiring: see `mcp.toml` alongside this file. Run `pie`, then `/triggers` in the REPL.

import json
import os
import sys
import threading
import urllib.parse
import urllib.request
from datetime import datetime, timezone

PROTOCOL_VERSION = "2025-03-26"
SERVER_NAME = "pie-weather"
SERVER_VERSION = "0.1.0"

LOCATION = os.environ.get("PIE_WEATHER_LOCATION", "San Francisco")
POLL_INTERVAL_SECS = int(os.environ.get("PIE_WEATHER_INTERVAL_SECS", "600"))
USER_AGENT = f"{SERVER_NAME}/{SERVER_VERSION} (+https://github.com/; stdlib urllib)"

_write_lock = threading.Lock()


def log(msg: str) -> None:
    # Logs MUST go to stderr — stdout is the JSON-RPC channel.
    print(f"[{SERVER_NAME}] {msg}", file=sys.stderr, flush=True)


def write_message(payload: dict) -> None:
    line = json.dumps(payload, separators=(",", ":"))
    with _write_lock:
        sys.stdout.write(line + "\n")
        sys.stdout.flush()


def send_response(request_id, result=None, error=None) -> None:
    msg = {"jsonrpc": "2.0", "id": request_id}
    if error is not None:
        msg["error"] = error
    else:
        msg["result"] = result if result is not None else {}
    write_message(msg)


def send_notification(method: str, params: dict) -> None:
    write_message({"jsonrpc": "2.0", "method": method, "params": params})


def fetch_weather(location: str) -> dict:
    # wttr.in returns a JSON document when asked with format=j1; no API key required.
    url = "https://wttr.in/%s?format=j1" % urllib.parse.quote(location)
    req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    with urllib.request.urlopen(req, timeout=15) as resp:
        return json.load(resp)


def summarize(doc: dict) -> tuple[str, str]:
    # Collapse the reading into (dedup_key, one-line sentence). Keying on the observation
    # timestamp means each distinct observation fires exactly once; re-polling the same
    # reading dedups away in the runtime.
    cc = doc["current_condition"][0]
    try:
        area = doc["nearest_area"][0]["areaName"][0]["value"]
    except (KeyError, IndexError):
        area = LOCATION
    desc = cc.get("weatherDesc", [{}])[0].get("value", "?")
    obs = cc.get("observation_time", "?")
    # Prefix with the UTC date so the same time-of-day on a later day is a distinct key
    # (otherwise tomorrow's "02:07 PM" would dedup against today's and be dropped).
    today = datetime.now(timezone.utc).date().isoformat()
    dedup_key = "obs:%s:%s" % (today, obs.replace(" ", ""))
    summary = (
        f"{area}: {cc.get('temp_C', '?')}°C "
        f"(feels {cc.get('FeelsLikeC', '?')}°C), {desc}, "
        f"humidity {cc.get('humidity', '?')}%, "
        f"wind {cc.get('windspeedKmph', '?')} km/h {cc.get('winddir16Point', '')}".rstrip()
        + f" — obs {obs}"
    )
    return dedup_key, summary


def poll_loop(stop_event: threading.Event) -> None:
    # Let the initialize handshake settle before the first push.
    if stop_event.wait(2.0):
        return
    last_key = None
    while not stop_event.is_set():
        try:
            doc = fetch_weather(LOCATION)
            dedup_key, summary = summarize(doc)
        except Exception as e:  # noqa: BLE001 - never let a transient fetch error kill the loop
            log(f"fetch failed: {type(e).__name__}: {e}")
        else:
            # Skip re-emitting an identical observation locally too (the runtime would
            # dedup it anyway via the key, but this keeps stdout quiet).
            if dedup_key != last_key:
                last_key = dedup_key
                now = datetime.now(timezone.utc).isoformat(timespec="seconds")
                send_notification(
                    "notifications/pie/weather/observation",
                    {
                        "_meta": {
                            # Per-observation key: re-polling the same reading does not re-fire.
                            # Namespaced by the runtime as
                            # `mcp:weather:custom:obs:<date>:<time>` (see McpNotificationHook).
                            "pie_dedup_key": dedup_key,
                            # The whole payload of this example: one sentence for the harness.
                            "pie_summary": summary,
                        },
                        # Dropped at the adapter (payload_visibility=Local); never persisted.
                        "fetched_at": now,
                    },
                )
                log(f"pushed: {summary}")
        if stop_event.wait(POLL_INTERVAL_SECS):
            return


def handle_request(method: str, params: dict, request_id) -> None:
    if method == "initialize":
        send_response(
            request_id,
            result={
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {"listChanged": False}},
                "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
            },
        )
        log("initialize -> ok")
        return
    if method == "tools/list":
        send_response(request_id, result={"tools": []})
        return
    if method == "tools/call":
        send_response(
            request_id,
            error={"code": -32601, "message": "no tools available on this server"},
        )
        return
    send_response(
        request_id,
        error={"code": -32601, "message": f"method not found: {method}"},
    )


def handle_notification(method: str, params: dict) -> None:
    if method == "notifications/initialized":
        log("client signaled initialized")
        return
    log(f"ignoring inbound notification: {method}")


def main() -> int:
    log(
        f"starting ({SERVER_NAME} v{SERVER_VERSION}); "
        f"location={LOCATION!r} interval={POLL_INTERVAL_SECS}s"
    )
    stop_event = threading.Event()
    poll_thread = threading.Thread(target=poll_loop, args=(stop_event,), daemon=True)
    poll_thread.start()

    try:
        for raw_line in sys.stdin:
            line = raw_line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except json.JSONDecodeError as e:
                log(f"bad json: {e}")
                continue
            method = msg.get("method")
            params = msg.get("params") or {}
            request_id = msg.get("id")
            if method is None:
                continue
            if request_id is None:
                handle_notification(method, params)
            else:
                handle_request(method, params, request_id)
    except KeyboardInterrupt:
        pass
    finally:
        stop_event.set()
        log("shutting down")
    return 0


if __name__ == "__main__":
    sys.exit(main())
