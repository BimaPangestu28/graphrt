"""The Supabase API gateway in front of a real PostgREST, for security_e2e.sh.

Supabase serves PostgREST under `/rest/v1/` behind a gateway that requires the
`apikey` header. This does the same: it refuses requests without the expected
publishable key, strips `/rest/v1` and forwards method, body and the
`Authorization` header to PostgREST, which verifies the JWT, switches to its role
and lets row level security decide.

Every request is appended to <log_file> as one JSON line with the path, the
request body, which known user the Bearer token belongs to (a label, never the
token), the status and the response body.

Standard library only.
Usage: python3 supabase_gateway.py <port> <postgrest_url> <publishable_key> <tokens_file> <log_file>
<tokens_file> has one `<label> <jwt>` per line.
"""

import json
import sys
import threading
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

FORWARDED_HEADERS = ("authorization", "content-type", "accept", "prefer", "content-profile", "accept-profile")
LOG_LOCK = threading.Lock()


def read_token_labels(tokens_path):
    labels = {}
    with open(tokens_path, encoding="utf-8") as tokens_file:
        for line in tokens_file:
            if line.strip():
                label, token = line.split(maxsplit=1)
                labels[token.strip()] = label
    return labels


def label_of(authorization, token_labels):
    if not authorization:
        return "none"
    token = authorization.removeprefix("Bearer ").strip()
    return token_labels.get(token, "unknown")


def forward(method, url, body, headers):
    request = urllib.request.Request(url, data=body if body else None, method=method, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=10) as response:
            return response.status, dict(response.headers), response.read()
    except urllib.error.HTTPError as error:
        return error.code, dict(error.headers), error.read()


def handler_for(postgrest_url, publishable_key, token_labels, log_path):
    class GatewayHandler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def handle_any(self):
            body = self.rfile.read(int(self.headers.get("content-length") or 0))
            if not self.path.startswith("/rest/v1/"):
                status, headers, payload = 404, {"content-type": "application/json"}, b'{"message":"no route"}'
            elif self.headers.get("apikey") != publishable_key:
                status, headers, payload = 401, {"content-type": "application/json"}, b'{"message":"Invalid API key"}'
            else:
                upstream_headers = {name: self.headers[name] for name in FORWARDED_HEADERS if self.headers.get(name)}
                status, headers, payload = forward(self.command, postgrest_url + self.path[len("/rest/v1"):], body, upstream_headers)
            self.record(body, status, payload)
            self.send_response(status)
            self.send_header("content-type", headers.get("Content-Type") or headers.get("content-type") or "application/json")
            self.send_header("content-length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def record(self, body, status, payload):
            entry = {"method": self.command, "path": self.path, "body": body.decode("utf-8", "replace"),
                     "bearer": label_of(self.headers.get("authorization"), token_labels),
                     "apikey_ok": self.headers.get("apikey") == publishable_key,
                     "status": status, "response": payload.decode("utf-8", "replace")}
            with LOG_LOCK, open(log_path, "a", encoding="utf-8") as log:
                log.write(json.dumps(entry) + "\n")

        do_GET = do_POST = do_PATCH = do_DELETE = do_PUT = handle_any

    return GatewayHandler


def main():
    port, postgrest_url, publishable_key, tokens_path, log_path = sys.argv[1:6]
    handler = handler_for(postgrest_url.rstrip("/"), publishable_key, read_token_labels(tokens_path), log_path)
    server = ThreadingHTTPServer(("127.0.0.1", int(port)), handler)
    print("gateway ready", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
