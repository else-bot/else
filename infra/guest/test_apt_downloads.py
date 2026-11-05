"""Exercise the guest's real APT downloader without reaching Debian.

Build IMAGE from this checkout's infra/guest/Dockerfile first.

    docker run --rm --network none -v "$PWD/infra/guest:/guest:ro" \
        --entrypoint python3 IMAGE /guest/test_apt_downloads.py
"""

import hashlib
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
import subprocess
import tempfile
import threading
import unittest


PAYLOAD = b"pinned package contents\n"


class AptDownloads(unittest.TestCase):
    def download(self, responses, payload=PAYLOAD):
        requests = []

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                status = responses[min(len(requests), len(responses) - 1)]
                requests.append(self.path)
                body = payload if status == 200 else b"snapshot temporarily unavailable\n"
                self.send_response(status)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *_):
                pass

        with HTTPServer(("127.0.0.1", 0), Handler) as server:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                with tempfile.TemporaryDirectory() as directory:
                    destination = Path(directory) / "package.deb"
                    result = subprocess.run(
                        ["/usr/lib/apt/apt-helper", "-o", "Acquire::Retries::Delay=false",
                         "download-file", f"http://127.0.0.1:{server.server_port}/package.deb",
                         str(destination), "SHA256:" + hashlib.sha256(PAYLOAD).hexdigest()],
                        capture_output=True, text=True, timeout=30,
                    )
                    contents = destination.read_bytes() if destination.exists() else None
            finally:
                server.shutdown()
                thread.join()
        return result, requests, contents

    def test_transient_errors_recover(self):
        for status in [429, 503, 504]:
            with self.subTest(status=status):
                result, requests, contents = self.download([status] * 4 + [200])
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(len(requests), 5)
                self.assertEqual(contents, PAYLOAD)

    def test_persistent_outage_exhausts_retries_and_fails(self):
        result, requests, _ = self.download([503])
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len(requests), 6)

    def test_missing_package_fails_without_retrying(self):
        result, requests, _ = self.download([404])
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len(requests), 1)

    def test_corrupt_package_is_rejected(self):
        result, _, _ = self.download([200], payload=b"corrupt contents")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Hash Sum mismatch", result.stderr)


if __name__ == "__main__":
    unittest.main()
