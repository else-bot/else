#!/usr/bin/env python3
"""Check the exported PR 60 artifacts. Native screenshots still need visual review."""
import argparse
import hashlib
import json
from pathlib import Path
import shlex
import sqlite3
import uuid

def check(condition):
    if not condition:
        raise ValueError("Recorded evidence failed a check.")


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("evidence", type=Path)
args = parser.parse_args()
root = args.evidence
export = root / "native/guest/evidence"
load = lambda path: json.loads(path.read_text())
proof = load(export / "after-cold-proof/proof.json")
saved = proof["saved"]
expected = [
    "[File: image.png]  Début 🥔 中[File: image.png] fin 😀  [File: image.png]",
    "[File: écran one.png]  between 🥔  [File: notes @Verification Assistant é 😀.txt][File: report space.pdf]",
    "New draft 🥔\n\n[File: écran one.png][File: notes @Verification Assistant é 😀.txt]"
    "[File: report space.pdf] @Attachment Buddy \n  /tmp/plain path.txt\nfile:///tmp/plain%20path.txt  \n"
    "[File: image.png] after 🥔 Main before [File: report space.pdf]",
    "".join(f"[File: limit {i}.txt]" for i in range(8)),
    "[File: Real PDF @Attachment Buddy.pdf]",
]
check([item["text"] for item in saved] == expected)
check(saved == load(export / "before-final-cold-proof/proof.json")["saved"])
check(saved[:1] == load(export / "image-proof/proof.json")["saved"])
api = load(export / "after-cold-proof/conversation.json")
expected_types = [
    ["file", "text", "file", "text", "file"],
    ["file", "text", "file", "file"],
    ["text", "file", "file", "file", "text", "file", "text", "file"],
    ["file"] * 8,
    ["file"],
]
references = 0
source_references = 0
screenshot_references = 0
with sqlite3.connect(f"file:{export / 'saved-state.sqlite3'}?mode=ro", uri=True) as db:
    for item, types in zip(saved, expected_types, strict=True):
        check([part["type"] for part in item["parts"]] == types)
        row = db.execute("SELECT text,parts FROM items WHERE id=?", (uuid.UUID(item["id"]).bytes,)).fetchone()
        check((row[0], json.loads(row[1])) == (item["text"], item["parts"]))
        remote = next(row for row in api["items"] if row["id"] == item["id"])
        check((remote["text"], remote["parts"]) == (item["text"], item["parts"]))
        for part in item["parts"]:
            if part["type"] != "file":
                continue
            file = part["file"]
            raw, sha = db.execute("SELECT data,sha256 FROM attachments WHERE id=?", (uuid.UUID(file["id"]).bytes,)).fetchone()
            downloaded = (export / "after-cold-proof" / file["id"]).read_bytes()
            check(downloaded == raw)
            check(len(raw) == file["size"])
            check(hashlib.sha256(raw).digest() == sha)
            source = export / "source-fixtures" / file["name"]
            if file["name"] == "image.png":
                check(not source.exists())
                screenshot_references += 1
            else:
                check(source.is_file())
                check(source.read_bytes() == raw)
                source_references += 1
            references += 1
check(references == 20)
check(source_references == 16)
check(screenshot_references == 4)
check(len(load(export / "image-proof/proof.json")["uploads"]) == 3)
requests = [json.loads(line) for line in (export / "fault-http.jsonl").read_text().splitlines()]
failed = next(row for row in requests if row["path"].endswith("/attachments") and row["status"] == 503)
retry = next(row for row in requests if row["path"].endswith("/attachments") and row["status"] == 201 and row["request"]["id"] == failed["request"]["id"])
check(failed["request"] == retry["request"])
rejected = next(row for row in requests if row["path"].endswith("/turns") and row["status"] == 429)["request"]
file_ids = lambda parts: [part["file"]["id"] for part in parts if part["type"] == "file"]
check(file_ids(saved[2]["parts"]) == file_ids(rejected["parts"]))
check(saved[2]["text"] == "New draft 🥔\n\n" + rejected["text"])
manifest = load(root / "native/manifest.json")
check(manifest["checkout"]["revision"] == "2efcf4ef1b1c83527843a08328094053cd9d1958")
check(manifest["checkout"]["dirty"] is False)
binary = load(export / "cold-binary.json")
check(binary["architecture"].endswith(": Mach-O 64-bit executable arm64"))
check(binary["sha256"] == manifest["payload"]["bin/else-desktop"]["sha256"])
check(manifest["phase"] == "cleaned")
check(not manifest["cleanup"]["absent"]["exists"])
check(all(row["stopped"] for row in load(export / "extra-process-cleanup.json").values()))
paths_run = root / "plain-path-native"
paths_export = paths_run / "guest/evidence"
paths = load(paths_export / "existing-path-proof.json")
expected_path = paths["expected"]
check(shlex.split(expected_path["path"]) == [expected_path["source_path"]])
check(expected_path["uri_text"] == "  " + expected_path["source_path"] + "\n" + Path(expected_path["source_path"]).as_uri() + "  ")
check(paths["item"]["text"] == expected_path["path"] + "\n" + expected_path["uri_text"])
check(paths["item"]["parts"] == [])
check((paths_export / "source-fixtures" / Path(expected_path["source_path"]).name).is_file())
with sqlite3.connect(f"file:{paths_export / 'saved-state.sqlite3'}?mode=ro", uri=True) as db:
    check(db.execute("SELECT count(*) FROM attachments").fetchone()[0] == 0)
    row = db.execute("SELECT text,parts FROM items WHERE kind='user'").fetchall()
    check(row == [(paths["item"]["text"], None)])
paths_manifest = load(paths_run / "manifest.json")
check(paths_manifest["checkout"]["revision"] == manifest["checkout"]["revision"])
check(paths_manifest["payload"] == manifest["payload"])
check(paths_manifest["phase"] == "cleaned")
check(not paths_manifest["cleanup"]["absent"]["exists"])
print("PASS: five attachment messages, 20 file references, existing-path text, exact bytes/order, retry IDs, cold restart, arm64 binary, and cleanup.")
