#!/usr/bin/env python3
"""Check installer connection URLs with disposable Tailscale and curl commands."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class Network(unittest.TestCase):
    def display(self, addresses, reachable):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for name, body in {
                "tailscale": 'printf "%s\\n" "$TEST_IPS"',
                "curl": 'for arg; do case "$arg" in http://*) url="$arg";; esac; done\n'
                        'case " $TEST_REACHABLE " in *" $url "*) exit 0;; *) exit 1;; esac',
            }.items():
                path = root / name
                path.write_text(f"#!/bin/bash\n{body}\n")
                path.chmod(0o755)
            env = {**os.environ, "PATH": f"{root}:{os.environ['PATH']}",
                   "ELSE_HOME": str(root / "else"), "ELSE_API_PORT": "9876",
                   "TEST_IPS": "\n".join(addresses), "TEST_REACHABLE": " ".join(reachable)}
            return subprocess.check_output(
                ["bash", "-c", 'source "$1"; elsebot() { :; }; start_api', "test", str(ROOT / "install.sh")],
                env=env, text=True,
            )

    def test_shows_local_and_reachable_tailscale_urls(self):
        output = self.display(["100.102.228.67", "fd7a:115c:a1e0::2"], [
            "http://127.0.0.1:9876/readyz", "http://100.102.228.67:9876/readyz",
            "http://[fd7a:115c:a1e0::2]:9876/readyz",
        ])
        self.assertIn("http://127.0.0.1:9876 (local)", output)
        self.assertIn("http://100.102.228.67:9876 (Tailscale)", output)
        self.assertIn("http://[fd7a:115c:a1e0::2]:9876 (Tailscale)", output)

    def test_does_not_advertise_an_unreachable_tailscale_listener(self):
        output = self.display(["100.102.228.67"], ["http://127.0.0.1:9876/readyz"])
        self.assertIn("http://127.0.0.1:9876 (local)", output)
        self.assertNotIn("Tailscale", output)

    def test_local_install_without_tailscale_addresses(self):
        output = self.display([], ["http://127.0.0.1:9876/readyz"])
        self.assertIn("http://127.0.0.1:9876 (local)", output)
        self.assertNotIn("Tailscale", output)


if __name__ == "__main__":
    unittest.main(verbosity=2)
