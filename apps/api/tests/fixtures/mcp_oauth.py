import base64
import hashlib
import json
import secrets
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import parse_qs, urlencode, urlsplit


class OAuthServer(HTTPServer):
    def __init__(self):
        super().__init__(("127.0.0.1", 0), Handler)
        self.base = f"http://127.0.0.1:{self.server_port}"
        self.clients = {}
        self.codes = {}
        self.access = set()
        self.refresh = set()
        self.revoked = False
        self.next_ttl = 3600
        self.events = []


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, body=None, headers=None):
        encoded = json.dumps(body).encode() if body is not None else b""
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(encoded)

    def event(self, kind, **fields):
        self.server.events.append({"kind": kind, **fields})

    def do_GET(self):
        path = urlsplit(self.path)
        server = self.server
        if path.path.startswith("/.well-known/"):
            self.event("discovery", path=path.path, authorization=bool(self.headers.get("Authorization")))
        if path.path in ("/.well-known/oauth-protected-resource", "/.well-known/oauth-protected-resource/mcp"):
            self.reply(200, {"resource": server.base + "/mcp", "authorization_servers": [server.base], "scopes_supported": ["fixture.read", "fixture.write"]})
        elif path.path == "/.well-known/oauth-authorization-server":
            self.reply(200, {
                "issuer": server.base,
                "authorization_endpoint": server.base + "/authorize",
                "token_endpoint": server.base + "/token",
                "registration_endpoint": server.base + "/register",
                "response_types_supported": ["code"],
                "grant_types_supported": ["authorization_code", "refresh_token"],
                "token_endpoint_auth_methods_supported": ["none"],
                "code_challenge_methods_supported": ["S256"],
                "authorization_response_iss_parameter_supported": True,
            })
        elif path.path == "/authorize":
            query = {key: values[0] for key, values in parse_qs(path.query).items()}
            client = server.clients.get(query.get("client_id"))
            if not client or query.get("redirect_uri") not in client["redirect_uris"] or query.get("response_type") != "code" or query.get("code_challenge_method") != "S256" or query.get("resource") != server.base + "/mcp":
                self.reply(400, {"error": "invalid_request"})
                return
            self.event("authorize", scope=query.get("scope"), resource=query["resource"], pkce=True)
            response = {"state": query["state"], "iss": server.base}
            if query.get("deny") == "true":
                response["error"] = "access_denied"
            else:
                code = secrets.token_urlsafe(24)
                server.codes[code] = query
                response["code"] = code
            separator = "&" if "?" in query["redirect_uri"] else "?"
            self.reply(302, headers={"Location": query["redirect_uri"] + separator + urlencode(response)})
        elif path.path == "/test":
            self.reply(200, {"events": server.events, "access_tokens": len(server.access), "refresh_tokens": len(server.refresh)})
        elif path.path == "/mcp":
            self.reply(405)
        else:
            self.reply(404)

    def do_POST(self):
        server = self.server
        raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        if self.path == "/token":
            form = {key: values[0] for key, values in parse_qs(raw.decode()).items()}
            kind = form.get("grant_type")
            self.event("token", grant_type=kind, resource=form.get("resource"), authorization=bool(self.headers.get("Authorization")))
            if form.get("resource") != server.base + "/mcp":
                self.reply(400, {"error": "invalid_target"})
                return
            if kind == "authorization_code":
                code = server.codes.pop(form.get("code"), None)
                challenge = base64.urlsafe_b64encode(hashlib.sha256(form.get("code_verifier", "").encode()).digest()).rstrip(b"=").decode()
                if not code or code["code_challenge"] != challenge or form.get("client_id") != code["client_id"] or form.get("redirect_uri") != code["redirect_uri"]:
                    self.reply(400, {"error": "invalid_grant"})
                    return
            elif kind == "refresh_token":
                refresh = form.get("refresh_token")
                if server.revoked or refresh not in server.refresh:
                    self.reply(400, {"error": "invalid_grant"})
                    return
                server.refresh.remove(refresh)
            else:
                self.reply(400, {"error": "unsupported_grant_type"})
                return
            access = "fixture-access-" + secrets.token_urlsafe(20)
            refresh = "fixture-refresh-" + secrets.token_urlsafe(20)
            server.access.add(access)
            server.refresh.add(refresh)
            self.reply(200, {"token_type": "Bearer", "access_token": access, "refresh_token": refresh, "expires_in": server.next_ttl, "scope": "fixture.read"})
        elif self.path == "/register":
            registration = json.loads(raw)
            self.event("register", redirect_uris=registration.get("redirect_uris"), auth_method=registration.get("token_endpoint_auth_method"), authorization=bool(self.headers.get("Authorization")))
            if registration.get("token_endpoint_auth_method") != "none" or not registration.get("redirect_uris"):
                self.reply(400, {"error": "invalid_client_metadata"})
                return
            client_id = "fixture-client-" + secrets.token_urlsafe(12)
            server.clients[client_id] = registration
            self.reply(201, {**registration, "client_id": client_id})
        elif self.path == "/test":
            control = json.loads(raw)
            if control.get("expire_access"):
                server.access.clear()
            if "revoked" in control:
                server.revoked = control["revoked"]
            if "next_ttl" in control:
                server.next_ttl = control["next_ttl"]
            self.reply(200, {})
        elif self.path == "/mcp":
            message = json.loads(raw)
            authorization = self.headers.get("Authorization", "")
            if not authorization.startswith("Bearer ") or authorization[7:] not in server.access:
                self.event("unauthorized", method=message.get("method"))
                self.reply(401, headers={"WWW-Authenticate": f'Bearer resource_metadata="{server.base}/.well-known/oauth-protected-resource", scope="fixture.read"'})
                return
            method = message.get("method")
            self.event("mcp", method=method)
            if "id" not in message:
                self.reply(202)
                return
            if method == "initialize":
                result = {"protocolVersion": message["params"]["protocolVersion"], "serverInfo": {"name": "oauth-fixture", "version": "1"}, "capabilities": {"tools": {}}}
            elif method == "tools/list":
                result = {"tools": [{"name": "fixture_read", "description": "Read a fixture value", "inputSchema": {"type": "object"}}]}
            elif method == "tools/call":
                result = {"content": [{"type": "text", "text": "fixture-value"}]}
            else:
                result = {}
            self.reply(200, {"jsonrpc": "2.0", "id": message["id"], "result": result})
        else:
            self.reply(404)

    def do_DELETE(self):
        self.reply(200)


server = OAuthServer()
print(server.base, flush=True)
server.serve_forever()
