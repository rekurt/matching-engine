"""Local test receiver; logs delivery without contacting an external recipient."""

from http.server import BaseHTTPRequestHandler, HTTPServer
import json


class Receiver(BaseHTTPRequestHandler):
    def do_POST(self):
        size = int(self.headers.get("Content-Length", "0"))
        if not 0 < size <= 1024 * 1024:
            self.send_error(413)
            return
        event = json.loads(self.rfile.read(size))
        print(json.dumps({"received": event}), flush=True)
        self.send_response(200)
        self.end_headers()


HTTPServer(("0.0.0.0", 8080), Receiver).serve_forever()
