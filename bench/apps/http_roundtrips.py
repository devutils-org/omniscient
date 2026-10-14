# SPDX-License-Identifier: MIT OR GPL-2.0
"""An HTTP server thread and N GETs against it, one connection each.

The workload of bench/apps/http-roundtrips.toml: N is the number of TCP
handshakes and of accepts the run makes, which is the ground truth.
"""

import http.server
import sys
import threading
import urllib.request


class Quiet(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b"ok\n"
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


def main():
    count = int(sys.argv[1]) if len(sys.argv) > 1 else 30
    server = http.server.HTTPServer(("127.0.0.1", 0), Quiet)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    url = f"http://127.0.0.1:{server.server_port}/"
    for _ in range(count):
        with urllib.request.urlopen(url) as response:
            response.read()
    server.shutdown()


if __name__ == "__main__":
    main()
