#!/usr/bin/env python3
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
BASH = shutil.which("bash")
IMAGE = "ghcr.io/else-bot/else-guest@sha256:" + "a" * 64
TOOLS = ["bash", "sed", "awk", "grep", "mktemp", "cat", "chmod", "mv", "tail", "mkdir",
         "rm", "ln", "env", "sort", "head", "tr", "touch", "basename", "jq"]
STUBS = {
    "sudo": 'case "$1" in -*) exit 0 ;; esac\nexec "$@"',
    "getent": "exit 1",
    "id": 'case "$*" in -u) echo 1000 ;; -un) echo tester ;; *) echo tester ;; esac',
    "dpkg-query": 'echo "install ok installed"',
    "pacman": "exit 0",
    "apt-get": "exit 0",
    "xcode-select": "exit 0",
    "xcrun": "exit 0",
    "msb": r'''
case "$1" in
image)
    [ "$STUB_MSB_STATUS" = 0 ] || exit "$STUB_MSB_STATUS"
    cat "$MSB_HOME/images.json" 2>/dev/null || echo '[]'
    ;;
pull)
    [ "$STUB_PULL_STATUS" = 0 ] || exit "$STUB_PULL_STATUS"
    printf '[{"reference":"%s"}]' "$2" >"$MSB_HOME/images.json"
    ;;
*) exit 2 ;;
esac
''',
    "docker": "exit 99",
    "systemctl": "exit 99",
    "sg": "exit 99",
    "usermod": "exit 99",
}
DRIVER = r'''
source "$INSTALLER"
DISTRO="$T_DISTRO" ARCH="$T_ARCH" SKIP_GUEST="$T_SKIP" ASSUME_YES=1 WANT_VERSION=1.0.0
source_version() { echo 1.0.0; }
installed_version() { echo 1.0.0; }
ensure_rust() { :; }
ensure_msb() {
    [ -x "$MSB" ] || ln -s "$STATE/msb" "$MSB"
    export STUB_MSB_STATUS="$STUB_REPAIRED_MSB_STATUS"
}
build_release() { :; }
switch_to() { :; }
install_commands() { :; }
setup_service() { :; }
run_setup() { :; }
api_running() { :; }
check_providers() { :; }
mkdir -p "$LOGS"
stage2
'''


class GuestImageDownloadTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.work = Path(directory.name)
        self.state = self.work / "state"
        self.stubs = self.work / "stubs"
        tools = self.work / "tools"
        self.home = self.work / "home"
        self.else_home = self.home / ".else"
        for path in [self.state, self.stubs, tools, self.else_home / "src/infra"]:
            path.mkdir(parents=True)
        for tool in TOOLS:
            (tools / tool).symlink_to(shutil.which(tool))
        for name, body in STUBS.items():
            target = self.else_home / "microsandbox/bin/msb" if name == "msb" else self.stubs / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(f'#!{BASH}\nprintf "%s\\n" "$(basename "$0") $*" >>"$STATE/calls"\n{body}\n')
            target.chmod(0o755)
        plutil = self.stubs / "plutil"
        plutil.write_text(f'#!{sys.executable}\n' + '''import json, sys
try:
    value = json.load(open(sys.argv[-1]))[sys.argv[2]]
    if value is None:
        sys.exit(1)
    print(value)
except (ValueError, KeyError):
    sys.exit(1)
''')
        plutil.chmod(0o755)
        self.manifest = self.else_home / "src/infra/guest-image.json"
        self.write_manifest()
        definitions, main = (ROOT / "install.sh").read_text().rstrip().rsplit("\n", 1)
        self.assertEqual(main, 'main "$@"')
        self.installer = self.work / "install-definitions.sh"
        self.installer.write_text(definitions + "\n")
        self.path = f"{self.stubs}:{tools}"

    def write_manifest(self, image=IMAGE, schema=1):
        self.manifest.write_text(json.dumps({"schema": schema, "source_sha256": "b" * 64, "image": image}))

    def run_stage2(self, distro="ubuntu", arch="x86_64", skip=False, msb_status=0,
                   pull_status=0, repaired_msb_status=None):
        (self.state / "calls").write_text("")
        env = {
            "PATH": self.path, "HOME": str(self.home), "ELSE_HOME": str(self.else_home), "USER": "tester",
            "STATE": str(self.state), "INSTALLER": str(self.installer),
            "T_DISTRO": distro, "T_ARCH": arch, "T_SKIP": "1" if skip else "0",
            "STUB_MSB_STATUS": str(msb_status), "STUB_PULL_STATUS": str(pull_status),
            "STUB_REPAIRED_MSB_STATUS": str(msb_status if repaired_msb_status is None else repaired_msb_status),
        }
        result = subprocess.run([BASH, "-c", DRIVER], env=env, text=True, capture_output=True, timeout=30)
        result.calls = (self.state / "calls").read_text().splitlines()
        config = self.else_home / "config/.env"
        result.config = config.read_text() if config.exists() else ""
        self.assertFalse([c for c in result.calls if c.split()[0] in {"docker", "sg", "systemctl", "usermod"}], result.calls)
        return result

    def load_image(self, reference=IMAGE):
        home = self.else_home / "data/microsandbox"
        home.mkdir(parents=True, exist_ok=True)
        (home / "images.json").write_text(json.dumps([{"reference": reference}]))

    def assertSucceeded(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_downloads_pinned_image_on_each_supported_platform_without_docker(self):
        for distro, arch in [("ubuntu", "x86_64"), ("debian", "aarch64"), ("arch", "x86_64"), ("macos", "aarch64")]:
            with self.subTest(distro=distro, arch=arch):
                self.setUp()
                result = self.run_stage2(distro=distro, arch=arch)
                self.assertSucceeded(result)
                self.assertIn("msb pull " + IMAGE, result.calls)
                self.assertIn(f"ELSE_VM_IMAGE={IMAGE}\n", result.config)
                self.assertIn("ELSE_VM_DESKTOP=1\n", result.config)
                self.assertIn("KVM" if distro != "macos" else "Xcode", result.stdout)
                self.assertEqual(json.loads((self.else_home / "data/microsandbox/images.json").read_text()),
                                 [{"reference": IMAGE}])

    def test_cached_image_skips_the_download(self):
        self.load_image()
        result = self.run_stage2()
        self.assertSucceeded(result)
        self.assertIn("already downloaded", result.stdout)
        self.assertFalse([c for c in result.calls if c.startswith("msb pull")])
        self.assertIn(f"ELSE_VM_IMAGE={IMAGE}", result.config)

    def test_old_alias_or_different_digest_does_not_skip_download(self):
        for reference in ["else-guest:3", IMAGE.replace("a" * 64, "c" * 64)]:
            with self.subTest(reference=reference):
                self.load_image(reference)
                result = self.run_stage2()
                self.assertSucceeded(result)
                self.assertIn("msb pull " + IMAGE, result.calls)

    def test_skip_needs_no_manifest_or_runtime(self):
        self.manifest.unlink()
        (self.else_home / "microsandbox/bin/msb").rename(self.state / "msb")
        result = self.run_stage2(skip=True)
        self.assertSucceeded(result)
        self.assertIn("not needed (no guest image)", result.stdout)
        self.assertNotIn("ELSE_VM_IMAGE", result.config)
        self.assertFalse([c for c in result.calls if c.startswith("msb")])

    def test_intel_mac_skips_guest_without_manifest(self):
        self.manifest.unlink()
        result = self.run_stage2(distro="macos", arch="x86_64")
        self.assertSucceeded(result)
        self.assertIn("need Apple silicon", result.stdout)
        self.assertNotIn("ELSE_VM_IMAGE", result.config)
        self.assertFalse([c for c in result.calls if c.startswith("msb")])

    def test_invalid_or_unpublished_image_cannot_be_used(self):
        for image in [None, "else-guest:3", "ghcr.io/else-bot/else-guest:latest", "--force", IMAGE + "\nextra"]:
            for distro, arch in [("ubuntu", "x86_64"), ("macos", "aarch64")]:
                with self.subTest(image=image, distro=distro):
                    self.setUp()
                    self.write_manifest(image)
                    result = self.run_stage2(distro=distro, arch=arch)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertNotIn("ELSE_VM_IMAGE", result.config)
                    self.assertFalse([c for c in result.calls if c.startswith("msb pull")])

    def test_unknown_schema_or_invalid_json_is_refused(self):
        for manifest in ["{", json.dumps({"schema": 2, "image": IMAGE})]:
            self.manifest.write_text(manifest)
            result = self.run_stage2()
            self.assertNotEqual(result.returncode, 0)
            self.assertNotIn("ELSE_VM_IMAGE", result.config)

    def test_failed_download_writes_no_guest_configuration_and_can_retry(self):
        failed = self.run_stage2(pull_status=1)
        self.assertNotEqual(failed.returncode, 0)
        self.assertIn("guest image download failed", failed.stderr)
        self.assertNotIn("ELSE_VM_IMAGE", failed.config)
        retried = self.run_stage2()
        self.assertSucceeded(retried)
        self.assertIn("msb pull " + IMAGE, retried.calls)

    def test_failed_lookup_stops_without_downloading(self):
        self.load_image()
        result = self.run_stage2(msb_status=1)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("could not list the guest images", result.stderr)
        self.assertFalse([c for c in result.calls if c.startswith("msb pull")])

    def test_runtime_repair_recovers_a_cached_image(self):
        self.load_image()
        result = self.run_stage2(msb_status=1, repaired_msb_status=0)
        self.assertSucceeded(result)
        self.assertIn("already downloaded", result.stdout)

    def test_rerun_after_download_skips_network(self):
        self.assertSucceeded(self.run_stage2())
        again = self.run_stage2(pull_status=1)
        self.assertSucceeded(again)
        self.assertFalse([c for c in again.calls if c.startswith("msb pull")])


if __name__ == "__main__":
    unittest.main()
