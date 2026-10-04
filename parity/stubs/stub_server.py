#!/usr/bin/env python3
"""Deterministic stub server for every upstream Octo talks to in the parity harness.

One process, several listeners. Each listener has a service name; the HTTPS and plain
HTTP listeners take the service name from the Host header instead, so a single TLS port
answers for api.deezer.com, itunes.apple.com and the rest (the containers resolve those
names to this one through docker network aliases, and trust the parity CA).

Mappings live in mappings/*.json: a JSON list of objects, tried in file-name order and
then in list order; the first match wins.

    {
      "service": "api.deezer.com",        # Host header, or slskd / lidarr / shim
      "method": "GET",                     # optional; any method when absent
      "path": "/search",                   # exact path, or
      "pathRegex": "^/album/\\d+$",        # a regex searched in the path
      "query": {"q": "(?i)aurora"},        # optional: each param must exist and match
      "status": 200,                       # default 200
      "headers": {"Content-Type": "..."},  # optional; JSON bodies default to application/json
      "body": {...},                       # a JSON value, serialised compactly, or
      "bodyText": "...",                   # a literal string, or
      "bodyFile": "bodies/tone.mp3",       # a file under the stubs dir (Range supported)
      "delayMs": 0                         # optional
    }

Unmatched requests answer 404 {"error":"no stub"} and are logged with UNMATCHED, so a
run's `docker compose logs stubs` shows any upstream call that still needs a mapping.
Stdlib only.
"""
from __future__ import annotations

import glob
import json
import os
import re
import ssl
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

HERE = os.path.dirname(os.path.abspath(__file__))
LOCK = threading.Lock()


def load_mappings() -> list[dict]:
    rows: list[dict] = []
    for path in sorted(glob.glob(os.path.join(HERE, "mappings", "*.json"))):
        with open(path, encoding="utf-8") as fh:
            for i, row in enumerate(json.load(fh)):
                row["_src"] = f"{os.path.basename(path)}#{i}"
                if "pathRegex" in row:
                    row["_re"] = re.compile(row["pathRegex"])
                row["_q"] = {k: re.compile(v) for k, v in (row.get("query") or {}).items()}
                rows.append(row)
    return rows


MAPPINGS = load_mappings()


def log(event: dict) -> None:
    with LOCK:
        sys.stdout.write(json.dumps(event, ensure_ascii=False) + "\n")
        sys.stdout.flush()


def match(service: str, method: str, path: str, query: dict[str, list[str]]) -> dict | None:
    for row in MAPPINGS:
        if row.get("service") != service:
            continue
        if row.get("method") and row["method"].upper() != method:
            continue
        if "path" in row and row["path"] != path:
            continue
        if "_re" in row and not row["_re"].search(path):
            continue
        ok = True
        for key, rx in row["_q"].items():
            values = query.get(key)
            if not values or not any(rx.search(v) for v in values):
                ok = False
                break
        if ok:
            return row
    return None


def make_handler(fixed_service: str | None):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"
        server_version = "parity-stub"
        sys_version = ""

        def log_message(self, fmt, *args):  # silence the default access log
            pass

        def _service(self) -> str:
            if fixed_service:
                return fixed_service
            host = (self.headers.get("Host") or "").split(":")[0].lower()
            return host

        def _handle(self) -> None:
            parts = urlsplit(self.path)
            query = parse_qs(parts.query, keep_blank_values=True)
            length = int(self.headers.get("Content-Length") or 0)
            body = self.rfile.read(length) if length > 0 else b""
            service = self._service()
            row = match(service, self.command, parts.path, query)
            log({
                "t": "UNMATCHED" if row is None else "hit",
                "service": service,
                "method": self.command,
                "path": parts.path,
                "query": parts.query,
                "body": body[:300].decode("utf-8", "replace") if body else None,
                "stub": row.get("_src") if row else None,
            })
            if row is None:
                self._send(404, {"Content-Type": "application/json"}, b'{"error":"no stub"}')
                return
            if row.get("delayMs"):
                time.sleep(row["delayMs"] / 1000.0)
            headers = dict(row.get("headers") or {})
            status = int(row.get("status", 200))
            if "body" in row:
                payload = json.dumps(row["body"], ensure_ascii=False, separators=(",", ":")).encode()
                headers.setdefault("Content-Type", "application/json")
            elif "bodyFile" in row:
                with open(os.path.join(HERE, row["bodyFile"]), "rb") as fh:
                    payload = fh.read()
                headers.setdefault("Content-Type", "application/octet-stream")
                headers.setdefault("Accept-Ranges", "bytes")
                rng = self.headers.get("Range")
                m = re.match(r"bytes=(\d*)-(\d*)$", rng or "")
                if m and status == 200 and (m.group(1) or m.group(2)):
                    total = len(payload)
                    if m.group(1):
                        start = int(m.group(1))
                        end = int(m.group(2)) if m.group(2) else total - 1
                    else:
                        start = max(0, total - int(m.group(2)))
                        end = total - 1
                    end = min(end, total - 1)
                    if start > end:
                        self._send(416, {"Content-Range": f"bytes */{total}"}, b"")
                        return
                    headers["Content-Range"] = f"bytes {start}-{end}/{total}"
                    payload = payload[start:end + 1]
                    status = 206
            else:
                payload = str(row.get("bodyText", "")).encode()
            self._send(status, headers, payload)

        def _send(self, status: int, headers: dict, payload: bytes) -> None:
            self.send_response(status)
            for k, v in headers.items():
                self.send_header(k, v)
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(payload)

        do_GET = do_POST = do_PUT = do_DELETE = do_PATCH = do_HEAD = do_OPTIONS = _handle

    return Handler


def serve(port: int, service: str | None, tls: bool) -> None:
    httpd = ThreadingHTTPServer(("0.0.0.0", port), make_handler(service))
    httpd.daemon_threads = True
    if tls:
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        certs = os.path.join(os.path.dirname(HERE), "certs")  # parity/certs, or /certs in the container
        ctx.load_cert_chain(os.path.join(certs, "stub.crt"), os.path.join(certs, "stub.key"))
        httpd.socket = ctx.wrap_socket(httpd.socket, server_side=True)
    httpd.serve_forever()


def main() -> None:
    # port:service[:tls]; a service of "*" routes by Host header.
    specs = sys.argv[1:] or ["443:*:tls", "80:*", "5030:slskd", "8686:lidarr", "8090:shim"]
    threads = []
    for spec in specs:
        bits = spec.split(":")
        port, service = int(bits[0]), (None if bits[1] == "*" else bits[1])
        tls = len(bits) > 2 and bits[2] == "tls"
        th = threading.Thread(target=serve, args=(port, service, tls), daemon=True)
        th.start()
        threads.append(th)
    log({"t": "ready", "listeners": specs, "mappings": len(MAPPINGS)})
    for th in threads:
        th.join()


if __name__ == "__main__":
    main()
