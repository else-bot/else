#!/usr/bin/env python3
"""Synthetic local research fixture. Never reads application state or credentials."""
import argparse
import copy
import ctypes
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def sync_dir(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def rename_no_replace(source, target):
    libc = ctypes.CDLL(None, use_errno=True)
    renameat2 = libc.renameat2
    renameat2.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint]
    renameat2.restype = ctypes.c_int
    if renameat2(-100, os.fsencode(source), -100, os.fsencode(target), 1):
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error), str(target))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--results", required=True, type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve()
    os.umask(0o077)
    scratch = Path("/tmp/elsebot-research-backups")
    scratch.mkdir(exist_ok=True)
    checks = []

    with tempfile.TemporaryDirectory(prefix="run-", dir=scratch) as directory:
        root = Path(directory)
        master, salt, wrong = [root / name for name in ("synthetic-master", "salt", "wrong-master")]
        for path in (master, salt, wrong):
            path.write_bytes(os.urandom(32))

        def crypto(mode, source, target, key=master):
            result = subprocess.run([str(binary), mode, str(key), str(salt), str(source), str(target)],
                                    capture_output=True, text=True)
            if result.returncode:
                raise ValueError("crypto rejected input: " + result.stderr.strip())

        crypto("key-check", master, salt)
        checks.append("HKDF repeatability, distinct purposes, and distinct salts")
        source = root / "source.bin"
        source.write_bytes(bytes(range(256)) * 8192 + b"end-of-stream-canary")
        store = root / "store"
        store.mkdir()
        ciphertext = store / "database.dump.age"
        crypto("encrypt", source, ciphertext)
        restored = root / "roundtrip.bin"
        crypto("decrypt", ciphertext, restored)
        assert sha256(source) == sha256(restored)
        checks.append("multi-chunk encrypted roundtrip, exact plaintext hash")
        second = root / "second.age"
        crypto("encrypt", source, second)
        assert sha256(second) != sha256(ciphertext)
        checks.append("same plaintext produces distinct ciphertext")

        manifest = {
            "schema_version": 1,
            "set_id": "synthetic-20261102-001",
            "created_at": "2026-11-02T00:00:00Z",
            "capture_started_at": "2026-11-02T00:00:00Z",
            "capture_finished_at": "2026-11-02T00:00:01Z",
            "encryption": {"format": "age-encryption.org/v1", "key_id": "fixture-master-v1",
                           "kdf": "HKDF-SHA-256", "info": "else.bot/backups/v1", "salt_hex": salt.read_bytes().hex()},
            "application": {"version": "synthetic", "migration_version": 1},
            "postgres": {"server_version": None, "dump_version": "synthetic-not-pg_dump", "format": "custom"},
            "artifacts": [{"kind": "postgres", "object": "database.dump.age", "restore_path": "database.dump",
                           "ciphertext_bytes": ciphertext.stat().st_size, "ciphertext_sha256": sha256(ciphertext),
                           "plaintext_bytes": source.stat().st_size, "plaintext_sha256": sha256(source)}],
            "bots": [],
            "coverage": {"postgres": "synthetic", "vm": "omitted-no-bots-in-fixture", "provider_state": "omitted-reauth-required"},
        }
        plain_manifest = root / "manifest.json"
        write_json(plain_manifest, manifest)
        crypto("encrypt", plain_manifest, store / "manifest.json.age")
        hint = {"schema_version": 1, "set_id": manifest["set_id"], "encryption": manifest["encryption"],
                "manifest_object": "manifest.json.age", "manifest_ciphertext_sha256": sha256(store / "manifest.json.age")}

        def publish(directory, expected):
            for artifact in expected["artifacts"]:
                path = directory / artifact["object"]
                if path.stat().st_size != artifact["ciphertext_bytes"] or sha256(path) != artifact["ciphertext_sha256"]:
                    raise ValueError("incomplete artifact")
            sync_dir(directory)
            temporary = directory / "complete.partial"
            write_json(temporary, hint)
            with temporary.open("rb") as stream:
                os.fsync(stream.fileno())
            os.link(temporary, directory / "complete.json")
            temporary.unlink()
            sync_dir(directory)

        def restore(directory, destination, key=master):
            if destination.exists():
                raise FileExistsError("restore destination already exists")
            outer = json.loads((directory / "complete.json").read_text())
            if outer["schema_version"] != 1 or outer["encryption"] != hint["encryption"]:
                raise ValueError("unsupported bootstrap")
            if outer["manifest_object"] != "manifest.json.age":
                raise ValueError("invalid manifest object")
            if sha256(directory / outer["manifest_object"]) != outer["manifest_ciphertext_sha256"]:
                raise ValueError("manifest ciphertext mismatch")
            stage = Path(tempfile.mkdtemp(prefix="restore-", dir=root))
            try:
                crypto("decrypt", directory / outer["manifest_object"], stage / "manifest.json", key)
                inner = json.loads((stage / "manifest.json").read_text())
                if inner["schema_version"] != 1 or inner["set_id"] != outer["set_id"] or inner["encryption"] != outer["encryption"]:
                    raise ValueError("manifest identity mismatch")
                names = set()
                for artifact in inner["artifacts"]:
                    for field in ("object", "restore_path"):
                        name = artifact[field]
                        if not name or name in (".", "..") or Path(name).name != name or name == "manifest.json":
                            raise ValueError("unsafe artifact path")
                    if artifact["restore_path"] in names:
                        raise ValueError("duplicate restore path")
                    names.add(artifact["restore_path"])
                    src, dst = directory / artifact["object"], stage / artifact["restore_path"]
                    if src.is_symlink() or src.stat().st_size != artifact["ciphertext_bytes"] or sha256(src) != artifact["ciphertext_sha256"]:
                        raise ValueError("ciphertext mismatch")
                    crypto("decrypt", src, dst, key)
                    if dst.stat().st_size != artifact["plaintext_bytes"] or sha256(dst) != artifact["plaintext_sha256"]:
                        raise ValueError("plaintext mismatch")
                rename_no_replace(stage, destination)
                sync_dir(destination.parent)
            finally:
                if stage.exists():
                    shutil.rmtree(stage)

        publish(store, manifest)
        original_completion = sha256(store / "complete.json")
        try:
            publish(store, manifest)
            raise AssertionError("completion record overwritten")
        except FileExistsError:
            pass
        assert sha256(store / "complete.json") == original_completion
        checks.append("repeat publication refuses to replace completed record")
        destination = root / "recovered"
        restore(store, destination)
        assert sha256(destination / "database.dump") == sha256(source)
        checks.append("complete manifest published last, verified restore to new destination")
        canary = destination / "owner-state"
        canary.write_text("preserve-owner-state")
        before = {p.name: sha256(p) for p in destination.iterdir()}
        try:
            restore(store, destination)
            raise AssertionError("existing destination accepted")
        except FileExistsError:
            pass
        assert before == {p.name: sha256(p) for p in destination.iterdir()}
        checks.append("existing restore destination refused, all file hashes unchanged")
        existing_output = root / "existing-output"
        existing_output.write_text("preserve-output")
        try:
            crypto("decrypt", ciphertext, existing_output)
            raise AssertionError("existing crypto output overwritten")
        except ValueError:
            pass
        assert existing_output.read_text() == "preserve-output"
        checks.append("crypto output creation refuses to replace an existing file")
        race_source, race_target = root / "rename-source", root / "rename-target"
        race_source.mkdir()
        race_target.mkdir()
        try:
            rename_no_replace(race_source, race_target)
            raise AssertionError("empty existing directory replaced")
        except FileExistsError:
            pass
        assert race_source.is_dir() and race_target.is_dir()
        checks.append("RENAME_NOREPLACE refuses an existing empty destination directory")

        def rejected(action, target):
            try:
                action()
            except (ValueError, FileNotFoundError):
                assert not target.exists()
                assert before == {p.name: sha256(p) for p in destination.iterdir()}
                return
            raise AssertionError("invalid backup accepted")

        rejected(lambda: restore(store, root / "wrong-key-restore", wrong), root / "wrong-key-restore")
        checks.append("wrong master rejected, no restore destination created")
        for name, mutate in (("late-bit-flip", lambda b: b[:-30] + bytes([b[-30] ^ 1]) + b[-29:]),
                             ("truncated", lambda b: b[:-1])):
            broken = root / (name + ".age")
            broken.write_bytes(mutate(ciphertext.read_bytes()))
            output = root / (name + ".plain")
            rejected(lambda: crypto("decrypt", broken, output), output)
            checks.append(name + " rejected by age, partial plaintext removed")

        incomplete = root / "incomplete"
        shutil.copytree(store, incomplete)
        (incomplete / "complete.json").unlink()
        (incomplete / "database.dump.age").unlink()
        rejected(lambda: publish(incomplete, manifest), incomplete / "complete.json")
        checks.append("missing producer artifact prevents complete publish")
        rejected(lambda: restore(incomplete, root / "partial-restore"), root / "partial-restore")
        checks.append("set without complete record refused")

        for case in ("unknown-schema", "path-traversal", "plaintext-hash-mismatch"):
            directory = root / case
            shutil.copytree(store, directory)
            changed = copy.deepcopy(manifest)
            if case == "unknown-schema":
                changed["schema_version"] = 2
            elif case == "path-traversal":
                changed["artifacts"][0]["restore_path"] = "../escaped"
            else:
                changed["artifacts"][0]["plaintext_sha256"] = "0" * 64
            write_json(plain_manifest, changed)
            (directory / "manifest.json.age").unlink()
            crypto("encrypt", plain_manifest, directory / "manifest.json.age")
            changed_hint = copy.deepcopy(hint)
            changed_hint["manifest_ciphertext_sha256"] = sha256(directory / "manifest.json.age")
            write_json(directory / "complete.json", changed_hint)
            rejected(lambda: restore(directory, root / (case + "-restore")), root / (case + "-restore"))
            assert not (root / "escaped").exists()
            checks.append(case + " refused before destination publish")

        changed_ciphertext = root / "corrupt-set"
        shutil.copytree(store, changed_ciphertext)
        (changed_ciphertext / "database.dump.age").write_bytes(b"corrupted")
        rejected(lambda: restore(changed_ciphertext, root / "corrupt-restore"), root / "corrupt-restore")
        checks.append("corrupt artifact rejected against authenticated manifest")
        result = {"status": "PASS", "date": "2026-11-02", "synthetic_only": True,
                  "encryption_library": "age 0.12.1", "format": "age-encryption.org/v1",
                  "plaintext_bytes": source.stat().st_size, "ciphertext_bytes": ciphertext.stat().st_size,
                  "plaintext_sha256": sha256(source), "checks": checks,
                  "check_count": len(checks), "postgres_dump_integration": "separate disposable integration check; see postgres-results.json",
                  "vm_roundtrip": "not rerun; reuse docs/experiments/bot-vm-verification.md",
                  "remote_upload": "UNVERIFIED: no credentials accessed and no storage calls made"}
        write_json(args.results, result)
        sample = copy.deepcopy(manifest)
        sample["encryption"]["salt_hex"] = "<32-byte public per-set salt as 64 hex characters>"
        write_json(args.results.with_name("manifest.example.json"), sample)
        print(json.dumps({"status": "PASS", "checks": len(checks), "results": str(args.results)}))


if __name__ == "__main__":
    main()
