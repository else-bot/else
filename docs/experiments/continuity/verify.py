#!/usr/bin/env python3
"""Offline research model. No providers, database, VM, or external effects."""
import json
import subprocess
import tempfile
from pathlib import Path


class Model:
    def __init__(self):
        self.messages = {}
        self.history = []
        self.sessions = {}
        self.generation = 0
        self.active = False
        self.actions = {}
        self.synthetic_effects = []

    def enqueue(self, request, text):
        if request in self.messages:
            assert self.messages[request] == text, "request reused with different input"
            return False
        self.messages[request] = text
        self.history.append((len(self.history) + 1, text))
        return True

    def open(self, provider, missing=False):
        if missing or provider not in self.sessions:
            old = self.sessions.get(provider, {"revision": 0})
            self.sessions[provider] = {
                "revision": old["revision"] + 1,
                "through": 0,
                "selected": [],
                "omitted": [],
            }
        return self.sessions[provider]

    def prepare(self, provider, budget_items=99, missing=False):
        assert not self.active, "old writer still active"
        session = self.open(provider, missing)
        missed = [seq for seq, _ in self.history if seq > session["through"]]
        selected = missed[-budget_items:] if budget_items else []
        return {
            "provider": provider,
            "revision": session["revision"],
            "through": len(self.history),
            "selected": selected,
            "omitted": [seq for seq in missed if seq not in selected],
            "delivery": "prepared",
        }

    def send(self, handoff):
        assert handoff["delivery"] == "prepared"
        handoff["delivery"] = "sending"

    def accept(self, handoff):
        assert handoff["delivery"] == "sending"
        session = self.sessions[handoff["provider"]]
        assert session["revision"] == handoff["revision"]
        session.update({key: handoff[key] for key in ["through", "selected", "omitted"]})
        handoff["delivery"] = "accepted"

    def fence(self):
        self.generation += 1
        self.active = False

    def action(self, action_id, generation, mode="chat", crash=False):
        assert generation == self.generation, "stale provider capability"
        assert mode == "chat", "summary has no tool capability"
        if action_id in self.actions:
            return self.actions[action_id]
        self.actions[action_id] = "unknown"
        self.synthetic_effects.append(action_id)
        if not crash:
            self.actions[action_id] = "done"
        return self.actions[action_id]


def rejects(fn):
    try:
        fn()
    except AssertionError:
        return
    raise AssertionError("operation unexpectedly accepted")


def cases():
    results = []
    m = Model()
    assert m.enqueue("r1", "start")
    assert not m.enqueue("r1", "start")
    rejects(lambda: m.enqueue("r1", "changed"))
    results.append("same request persists one owner message; conflicting reuse rejected")

    c = m.prepare("codex")
    m.send(c)
    m.accept(c)
    m.enqueue("r2", "continue in Claude")
    h = m.prepare("claude")
    assert h["selected"] == [1, 2]
    m.send(h)
    m.accept(h)
    m.enqueue("r3", "return to Codex")
    h = m.prepare("codex")
    assert h["selected"] == [2, 3]
    results.append("Codex to Claude to Codex selects only returning-session delta")

    h = m.prepare("codex", missing=True)
    assert h["selected"] == [1, 2, 3]
    assert h["revision"] == 2
    results.append("missing native session resets coverage and restores full candidate history")

    h = m.prepare("claude", budget_items=0)
    assert h["selected"] == [] and h["omitted"] == [3]
    m.send(h)
    assert m.sessions["claude"]["through"] == 2
    m.accept(h)
    assert m.sessions["claude"]["through"] == 3
    assert m.sessions["claude"]["omitted"] == [3]
    results.append("considered-through cursor advances only on acceptance; omissions remain explicit")

    h = m.prepare("codex")
    m.send(h)
    h["delivery"] = "uncertain"
    old_revision = h["revision"]
    replacement = m.prepare("codex", missing=True)
    assert replacement["revision"] > old_revision
    assert replacement["selected"] == [1, 2, 3]
    rejects(lambda: m.send(h))
    results.append("uncertain send cannot be resent; replacement receives full candidate history")

    m.active = True
    rejects(lambda: m.prepare("claude"))
    old_generation = m.generation
    m.fence()
    rejects(lambda: m.action("late", old_generation))
    assert not m.synthetic_effects
    results.append("switch waits for old writer; revoked generation blocks late tool calls")

    rejects(lambda: m.action("summary-side-effect", m.generation, mode="checkpoint"))
    assert not m.synthetic_effects
    results.append("checkpoint has no scoped tool execution capability")

    assert m.action("job-1", m.generation, crash=True) == "unknown"
    assert m.action("job-1", m.generation) == "unknown"
    assert m.synthetic_effects == ["job-1"]
    m.actions["job-1"] = "done"
    assert m.action("job-1", m.generation) == "done"
    assert m.synthetic_effects == ["job-1"]
    results.append("lost action result stays unknown; same action ID never repeats dispatch")
    return results


def probe():
    """Only version, help, and schema generation. Never read provider credentials."""
    def run(*args):
        return subprocess.run(args, check=True, capture_output=True, text=True).stdout

    codex_version = run("codex", "--version").strip()
    claude_version = run("claude", "--version").strip()
    help_text = run("claude", "--help")
    flags = ["--resume", "--session-id", "--input-format", "--output-format",
             "--replay-user-messages", "--tools", "--strict-mcp-config",
             "--restricted", "--system-prompt-snapshot"]
    assert all(flag in help_text for flag in flags)
    with tempfile.TemporaryDirectory(prefix="elsebot-continuity-schema-") as directory:
        run("codex", "app-server", "generate-json-schema", "--out", directory)
        schemas = {}
        for name in ["ThreadStartParams", "ThreadResumeParams", "TurnStartParams",
                     "TurnInterruptParams", "TurnCompletedNotification"]:
            data = json.loads((Path(directory) / "v2" / (name + ".json")).read_text())
            schemas[name] = {
                "required": data.get("required", []),
                "fields": sorted(data.get("properties", {})),
            }
        assert schemas["ThreadResumeParams"]["required"] == ["threadId"]
        assert schemas["TurnInterruptParams"]["required"] == ["threadId", "turnId"]
        assert schemas["TurnStartParams"]["required"] == ["input", "threadId"]
    return {"codex": codex_version, "claude": claude_version,
            "claude_flags_present": flags, "codex_schemas": schemas}


if __name__ == "__main__":
    passed = cases()
    print(json.dumps({"status": "PASS", "case_count": len(passed), "cases": passed,
                      "offline_cli_probe": probe(), "live_provider_calls": 0,
                      "limits": "Synthetic state model and local CLI shape checks only. "
                                "No provider delivery, auth, interruption, or OS isolation proof."},
                     indent=2))
