#!/usr/bin/env python3
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
BASH = shutil.which("bash")
TOOLS = ["bash", "sed", "awk", "grep", "mktemp", "cat", "chmod", "mv", "tail", "mkdir",
         "rm", "ln", "env", "sort", "head", "tr", "touch", "basename"]
TAG = "else-guest:3"

STUBS = {
    "sudo": 'case "$1" in -*) exit 0 ;; esac\nexec "$@"',
    "systemctl": '[ "$1 $2" = "is-active --quiet" ] && exit "$STUB_DOCKER_ACTIVE"\nexit 0',
    "docker": '[ "$1" = info ] && exit "$STUB_DOCKER_INFO"\nexit 0',
    "sg": 'export STUB_UNDER_SG=1\nexec bash -c "$3"',
    "getent": '[ "$2" = docker ] && exit "$STUB_DOCKER_GROUP"\nexit 1',
    "usermod": 'echo "$2" >>"$STATE/user_groups"',
    "id": 'case "$*" in\n-u) echo 1000 ;;\n-un) echo tester ;;\n-nG) cat "$STATE/session_groups" ;;\n*) tr "\\n" " " <"$STATE/user_groups" ;;\nesac',
    "dpkg-query": 'case " $STUB_MISSING " in *" ${!#} "*) exit 1 ;; esac\necho "install ok installed"',
    "pacman": 'case "$1" in -Q*) case " $STUB_MISSING " in *" $2 "*) exit 1 ;; esac ;; esac\n'
              'case " $* " in *" docker "*) ln -sf "$STATE/docker" "$STUBS/docker" ;; esac',
    "apt-get": 'case " $* " in *" docker.io "*) ln -sf "$STATE/docker" "$STUBS/docker" ;; esac',
    "xcode-select": "exit 0",
    "xcrun": "exit 0",
    "msb": '[ "$*" = "image list --format json" ] || exit 2\n[ "$STUB_MSB_STATUS" = 0 ] || exit "$STUB_MSB_STATUS"\n'
           'cat "$MSB_HOME/images.json" 2>/dev/null || echo "[]"',
    "build-guest-image.sh": '[ "$STUB_BUILD_STATUS" = 0 ] || exit "$STUB_BUILD_STATUS"\n'
                            'mkdir -p "$1/microsandbox"\nprintf \'[{"reference":"%s"}]\' "$ELSE_GUEST_TAG" >"$1/microsandbox/images.json"',
}

DRIVER = r'''
source "$INSTALLER"
DISTRO="$T_DISTRO" ARCH="$T_ARCH" SKIP_GUEST="$T_SKIP" ASSUME_YES=1 WANT_VERSION=1.0.0
has_systemd() { [ "$T_SYSTEMD" = 1 ]; }
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


class GuestImageDockerTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.work = Path(directory.name)
        self.state = self.work / "state"
        self.stubs = self.work / "stubs"
        tools = self.work / "tools"
        self.home = self.work / "home"
        self.else_home = self.home / ".else"
        for path in [self.state, self.stubs, tools, self.else_home / "src/scripts"]:
            path.mkdir(parents=True)
        for tool in TOOLS:
            (tools / tool).symlink_to(shutil.which(tool))

        def stub(path, body):
            log = 'printf "%s\\n" "$(basename "$0") $*" >>"$STATE/calls"\n'
            path.write_text(f"#!{BASH}\n{log}{body}\n")
            path.chmod(0o755)

        for name, body in STUBS.items():
            target = {"docker": self.state / "docker",
                      "msb": self.else_home / "microsandbox/bin/msb",
                      "build-guest-image.sh": self.else_home / "src/scripts/build-guest-image.sh"}.get(name, self.stubs / name)
            target.parent.mkdir(parents=True, exist_ok=True)
            stub(target, body)
        for name in ["user_groups", "session_groups", "calls"]:
            (self.state / name).touch()

        text = (ROOT / "install.sh").read_text()
        body, main = text.rstrip().rsplit("\n", 1)
        self.assertEqual(main, 'main "$@"')
        self.installer = self.work / "install-definitions.sh"
        self.installer.write_text(body + "\n")
        self.path = f"{self.stubs}:{tools}"

    def run_stage2(self, distro="ubuntu", arch="x86_64", skip=False, docker=False, docker_running=False,
                   docker_info=0, docker_group=False, systemd=True, msb_status=0, build_status=0, groups="",
                   repaired_msb_status=None):
        (self.state / "calls").write_text("")
        (self.state / "user_groups").write_text(groups)
        (self.state / "session_groups").write_text(groups)
        if docker:
            (self.stubs / "docker").unlink(missing_ok=True)
            (self.stubs / "docker").symlink_to(self.state / "docker")
        env = {
            "PATH": self.path, "HOME": str(self.home), "ELSE_HOME": str(self.else_home), "USER": "tester",
            "STATE": str(self.state), "STUBS": str(self.stubs), "INSTALLER": str(self.installer),
            "T_DISTRO": distro, "T_ARCH": arch, "T_SKIP": "1" if skip else "0", "T_SYSTEMD": "1" if systemd else "0",
            "STUB_MISSING": "docker.io docker", "STUB_DOCKER_ACTIVE": "0" if docker_running else "1",
            "STUB_DOCKER_INFO": str(docker_info), "STUB_DOCKER_GROUP": "0" if docker_group else "1",
            "STUB_MSB_STATUS": str(msb_status), "STUB_BUILD_STATUS": str(build_status),
            "STUB_REPAIRED_MSB_STATUS": str(msb_status if repaired_msb_status is None else repaired_msb_status),
        }
        result = subprocess.run([BASH, "-c", DRIVER], env=env, text=True, capture_output=True, timeout=60)
        result.calls = (self.state / "calls").read_text().splitlines()
        env_file = self.else_home / "config/.env"
        result.config = env_file.read_text() if env_file.exists() else ""
        return result

    def load_image(self, tag=TAG):
        images = self.else_home / "data/microsandbox"
        images.mkdir(parents=True)
        (images / "images.json").write_text(f'[{{"reference":"{tag}"}}]')

    def docker_calls(self, result):
        return [c for c in result.calls
                if c.split()[0] in {"docker", "sg", "usermod", "getent", "systemctl", "build-guest-image.sh"}
                or "docker" in c.split()]

    def assertSucceeded(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_skipped_or_loaded_image_never_touches_docker(self):
        for distro, arch in [("ubuntu", "x86_64"), ("arch", "x86_64"), ("macos", "aarch64")]:
            for skip in [True, False]:
                with self.subTest(distro=distro, skip=skip):
                    self.setUp()
                    if not skip:
                        self.load_image()
                    result = self.run_stage2(distro=distro, arch=arch, skip=skip)
                    self.assertSucceeded(result)
                    self.assertEqual(self.docker_calls(result), [])
                    if skip:
                        self.assertIn("not needed (no guest image)", result.stdout)
                        self.assertNotIn("ELSE_VM_IMAGE", result.config)
                    else:
                        self.assertIn("already loaded", result.stdout)
                        self.assertIn(f"ELSE_VM_IMAGE={TAG}", result.config)
                    self.assertIn("KVM" if distro != "macos" else "Xcode", result.stdout)

    def test_missing_image_installs_starts_and_builds_without_enabling(self):
        for distro, install in [("debian", "apt-get install -y -qq --no-install-recommends docker.io"),
                                ("ubuntu", "apt-get install -y -qq --no-install-recommends docker.io"),
                                ("arch", "pacman -Sy --needed --noconfirm docker")]:
            with self.subTest(distro=distro):
                self.setUp()
                result = self.run_stage2(distro=distro)
                self.assertSucceeded(result)
                self.assertTrue(any(c.endswith(install) for c in result.calls), result.calls)
                self.assertIn("sudo systemctl start docker", result.calls)
                self.assertFalse([c for c in result.calls if " enable" in c or " disable" in c])
                self.assertIn("build-guest-image.sh " + str(self.else_home / "data"), result.calls)
                self.assertIn(f"ELSE_VM_IMAGE={TAG}", result.config)

    def test_without_systemd_the_service_is_left_alone(self):
        result = self.run_stage2(docker=True, systemd=False)
        self.assertSucceeded(result)
        self.assertFalse([c for c in result.calls if c.startswith("systemctl")])
        self.assertIn("build-guest-image.sh " + str(self.else_home / "data"), result.calls)

    def test_an_older_loaded_image_does_not_skip_the_requested_build(self):
        self.load_image("else-guest:1")
        result = self.run_stage2(docker=True)
        self.assertSucceeded(result)
        self.assertIn("docker info", result.calls)
        self.assertIn("build-guest-image.sh " + str(self.else_home / "data"), result.calls)
        self.assertIn(f"ELSE_VM_IMAGE={TAG}", result.config)

    def test_new_docker_group_runs_the_whole_build_through_sg(self):
        result = self.run_stage2(docker=True, docker_running=True, docker_group=True)
        self.assertSucceeded(result)
        self.assertIn("usermod -aG docker tester", result.calls)
        sg = [c for c in result.calls if c.startswith("sg docker -c")]
        self.assertEqual(len(sg), 2, result.calls)
        self.assertTrue(sg[0].endswith("docker info"))
        self.assertIn("build-guest-image.sh", sg[1])
        self.assertIn("ELSE_GUEST_TAG=" + TAG, sg[1])

    def test_macos_needs_running_docker_only_to_build(self):
        for docker, info, message in [(False, 0, "Docker is missing"), (True, 1, "Docker is not running")]:
            with self.subTest(message=message):
                self.setUp()
                result = self.run_stage2(distro="macos", arch="aarch64", docker=docker, docker_info=info)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(message, result.stderr)
                self.assertFalse([c for c in result.calls if c.startswith("build-guest-image.sh")])

    def test_intel_mac_skips_the_image_without_asking_msb(self):
        self.load_image()
        result = self.run_stage2(distro="macos", arch="x86_64")
        self.assertSucceeded(result)
        self.assertIn("need Apple silicon", result.stdout)
        self.assertIn("not needed (no guest image)", result.stdout)
        self.assertEqual(self.docker_calls(result), [])
        self.assertFalse([c for c in result.calls if c.startswith("msb")])

    def test_failed_image_lookup_stops_before_docker(self):
        self.load_image()
        result = self.run_stage2(msb_status=1)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("could not list the guest images", result.stderr)
        self.assertEqual(self.docker_calls(result), [])
        self.assertFalse([c for c in result.calls if c.startswith(("apt-get", "dpkg-query"))])

    def test_runtime_repair_can_recover_a_loaded_image_without_docker(self):
        self.load_image()
        result = self.run_stage2(msb_status=1, repaired_msb_status=0)
        self.assertSucceeded(result)
        self.assertIn("already loaded", result.stdout)
        self.assertIn(f"ELSE_VM_IMAGE={TAG}", result.config)
        self.assertEqual(self.docker_calls(result), [])

    def test_missing_runtime_can_recover_a_loaded_image_without_docker(self):
        self.load_image()
        (self.else_home / "microsandbox/bin/msb").rename(self.state / "msb")
        result = self.run_stage2()
        self.assertSucceeded(result)
        self.assertIn("already loaded", result.stdout)
        self.assertIn(f"ELSE_VM_IMAGE={TAG}", result.config)
        self.assertEqual(self.docker_calls(result), [])

    def test_failed_build_writes_no_guest_configuration(self):
        result = self.run_stage2(docker=True, build_status=1)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("the guest image build failed", result.stderr)
        self.assertNotIn("ELSE_VM_IMAGE", result.config)

    def test_rerun_after_a_build_skips_docker(self):
        self.assertSucceeded(self.run_stage2(docker=True))
        (self.stubs / "docker").unlink()
        result = self.run_stage2()
        self.assertSucceeded(result)
        self.assertIn("already loaded", result.stdout)
        self.assertEqual(self.docker_calls(result), [])


if __name__ == "__main__":
    unittest.main()
