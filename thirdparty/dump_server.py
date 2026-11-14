import http.server, json
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(n).decode("utf-8", "ignore")
        with open(r"D:\ZCodefiles\.zcode\workspace\draft-204095a0-ce0c-40d4-8f95-4322eff76e08\dom_dump.json", "a", encoding="utf-8") as f:
            f.write(body + "\n")
        self.send_response(204)
        self.end_headers()
    def log_message(self, *a):
        pass
http.server.HTTPServer(("127.0.0.1", 18080), H).serve_forever()
