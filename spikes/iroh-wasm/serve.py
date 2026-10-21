#!/usr/bin/env python3
"""Static server for www/ on localhost; POST /result writes result.json."""
import http.server
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.environ.get("SPIKE_RESULT", os.path.join(HERE, "result.json"))


class Handler(http.server.SimpleHTTPRequestHandler):
    extensions_map = {**http.server.SimpleHTTPRequestHandler.extensions_map, ".wasm": "application/wasm", ".js": "text/javascript"}

    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=os.path.join(HERE, "www"), **kwargs)

    def end_headers(self):
        self.send_header("Cache-Control", "no-store")
        super().end_headers()

    def do_POST(self):
        if self.path != "/result":
            self.send_error(404)
            return
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        with open(OUT, "wb") as f:
            f.write(body)
        self.send_response(204)
        self.end_headers()


port = int(sys.argv[1]) if len(sys.argv) > 1 else 8765
http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
