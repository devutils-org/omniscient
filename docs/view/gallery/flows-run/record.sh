#!/bin/sh
# Records the run the flows view is judged on, through `omn top` itself.
#
#   make && docs/view/gallery/flows-run/record.sh
#
# The common fixtures (`tests/view_fixtures/`) carry `io.syscall` as a summary
# and a histogram -- latency per thread, no bytes -- and no `io.fs` at all, so
# none of them says which file a byte came from. This run asks both as `raw`,
# which is what the flows view reads bytes per file from, beside the network
# and the machine's devices and interfaces.
#
# The workload is a small file server: CPython's `http.server`, threaded, with
# a durable access log (one fsync per request) and uploads written to disk and
# fsynced. A client in another process -- not the target -- downloads three
# files of three sizes and uploads one, twelve times, on one keep-alive
# connection to loopback, then asks the server to stop. Every flow the view
# draws is there: bytes received and sent on a socket, read from and written
# to files, moved by a device, carried by an interface.
#
# Needs the capabilities `make` grants (the eBPF sources) and python3.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)
omn="$root/target/debug/omn"
# On a disk rather than in memory, so a write has a device under it.
work="$root/target/omn-flows-run"
rm -rf "$work"
mkdir -p "$work/www" "$work/uploads"

python3 - "$work/www" <<'PY'
import os, sys
www = sys.argv[1]
for name, size in (("video.bin", 8 << 20), ("app.js", 1 << 20), ("index.html", 64 << 10)):
    with open(os.path.join(www, name), "wb") as file:
        file.write(os.urandom(size))
PY

cat >"$work/server.py" <<'PY'
import http.server, os, sys, threading

www, uploads, log_path, port = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
log = open(log_path, "a")
lock = threading.Lock()

class Handler(http.server.SimpleHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=www, **kwargs)

    def log_message(self, fmt, *args):
        with lock:
            log.write("%s %s\n" % (self.address_string(), fmt % args))
            log.flush()
            os.fsync(log.fileno())

    def do_PUT(self):
        if self.path == "/quit":
            self.send_response(204)
            self.send_header("Content-Length", "0")
            self.end_headers()
            threading.Thread(target=self.server.shutdown).start()
            return
        length = int(self.headers["Content-Length"])
        name = os.path.join(uploads, os.path.basename(self.path))
        with open(name, "wb") as file:
            left = length
            while left:
                chunk = self.rfile.read(min(left, 65536))
                file.write(chunk)
                left -= len(chunk)
            file.flush()
            os.fsync(file.fileno())
        self.send_response(201)
        self.send_header("Content-Length", "0")
        self.end_headers()

http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
PY

cat >"$work/client.py" <<'PY'
import http.client, os, sys, time

port = int(sys.argv[1])
for _ in range(200):
    try:
        connection = http.client.HTTPConnection("127.0.0.1", port)
        connection.connect()
        break
    except OSError:
        time.sleep(0.02)
body = os.urandom(256 << 10)
for round in range(12):
    for name in ("video.bin", "app.js", "index.html"):
        connection.request("GET", "/" + name)
        connection.getresponse().read()
    connection.request("PUT", "/upload-%02d.bin" % round, body)
    connection.getresponse().read()
    time.sleep(0.15)
connection.request("PUT", "/quit")
connection.getresponse().read()
PY

port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')

cat >"$work/session.yml" <<YML
name: flows
duration: 20s
format: json
requests:
- {source: net.socket, label: net.socket, mode: trace, emit: [raw, hist],
   target: &server {launch: [sh, -c, "python3 $work/client.py $port & exec python3 $work/server.py $work/www $work/uploads $work/access.log $port"]}}
- {source: io.syscall, label: io.syscall, mode: trace, emit: [raw, summary, hist], target: *server}
- {source: io.fs, label: io.fs, mode: trace, emit: [raw], target: *server}
- {source: net.bytes, label: net.bytes@system, mode: poll@250ms, emit: [timeline, summary], target: system}
- {source: io.bytes, label: io.bytes@system, mode: poll@250ms, emit: [timeline, summary], target: system}
YML

out="$here/data"
rm -rf "$out"
"$omn" top --headless -f "$work/session.yml" -o "$out" >/dev/null
rm -f "$out/resolved.json" "$out/cost.json"
sed -i "s|$work|/work|g; s|$root|/omniscient|g" "$out/results.json" "$out/view.json"
python3 -c 'import json, sys; json.dump(json.load(open(sys.argv[1])), open(sys.argv[1], "w"), separators=(",", ":"))' \
    "$out/results.json"
echo "recorded flows-run: $(du -sh "$out" | cut -f1)"
