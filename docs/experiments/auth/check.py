#!/usr/bin/env python3
"""Synthetic policy checks. No server, provider CLI, credentials, or network."""

import base64
import hashlib
import hmac
import pathlib
import re
import subprocess
import tempfile
from dataclasses import dataclass, replace


def token(byte):
    return base64.urlsafe_b64encode(bytes([byte]) * 32).decode().rstrip("=")


def digest(value):
    return hashlib.sha256(value.encode()).digest()


@dataclass(frozen=True)
class Grant:
    bot: str = "a"
    task: str = "ta"
    conversation: str = "ca"
    session: int = 1
    expires: int = 200
    tools: frozenset = frozenset({"conversation_read", "guest_exec", "job_read"})
    revoked: bool = False


class Policy:
    def __init__(self):
        self.owner = digest(token(1))
        self.grants = {digest(token(2)): Grant()}
        self.tasks = {"ta": ("a", "ca", 1, True)}
        self.bots = {"a": "active", "b": "active"}
        self.computer_stopped = {"a": False, "b": False}
        self.vm_generation = {"a": 1, "b": 1}
        self.jobs = {"ja": ("a", "ta"), "jb": ("b", "tb")}
        self.started = []

    def authenticate(self, authorization, now=100):
        if len(authorization) != 1:
            return None
        match = re.fullmatch(r"Bearer ([A-Za-z0-9_-]{43})", authorization[0], re.I)
        if not match:
            return None
        value = match.group(1)
        raw = base64.urlsafe_b64decode(value + "=")
        if base64.urlsafe_b64encode(raw).decode().rstrip("=") != value:
            return None
        hashed = digest(value)
        if hmac.compare_digest(hashed, self.owner):
            return "owner"
        grant = self.grants.get(hashed)
        if grant is None or grant.revoked or now >= grant.expires:
            return None
        return grant

    def allowed(self, principal, action, bot="a", task="ta", conversation="ca", job=None):
        if principal == "owner":
            return action in {"list_bots", "create_bot", "delete_bot", "conversation_read"}
        if not isinstance(principal, Grant) or action not in principal.tools:
            return False
        if (bot, task, conversation) != (principal.bot, principal.task, principal.conversation):
            return False
        current = self.tasks.get(task)
        if current != (bot, conversation, principal.session, True):
            return False
        if self.bots.get(bot) != "active":
            return False
        if action == "guest_exec" and self.computer_stopped[bot]:
            return False
        if action == "job_read" and self.jobs.get(job) != (bot, task):
            return False
        return True

    def request(self, headers, action, **kwargs):
        return self.allowed(self.authenticate(headers), action, **kwargs)

    def prepare(self):
        return (token(2), self.vm_generation["a"])

    def dispatch(self, prepared):
        bearer, generation = prepared
        principal = self.authenticate(["Bearer " + bearer])
        if generation != self.vm_generation["a"]:
            return False
        if not self.allowed(principal, "guest_exec"):
            return False
        self.started.append("guest operation accepted")
        return True

    def stop_computer(self):
        self.computer_stopped["a"] = True
        self.vm_generation["a"] += 1

    def delete_bot(self):
        self.bots["a"] = "deleting"
        self.vm_generation["a"] += 1


def main():
    passed = 0

    def check(name, actual, expected):
        nonlocal passed
        assert actual == expected, name
        passed += 1
        print(f"PASS {name}")

    owner = ["Bearer " + token(1)]
    runtime = ["Bearer " + token(2)]
    p = Policy()
    check("missing bearer denied without bypass", p.request([], "list_bots"), False)
    check("unknown bearer denied", p.request(["Bearer " + token(3)], "list_bots"), False)
    check("duplicate authorization denied", p.request(owner + owner, "list_bots"), False)
    check("malformed bearer denied", p.request(["Bearer short"], "list_bots"), False)
    check("wrong auth scheme denied", p.request(["Basic " + token(1)], "list_bots"), False)
    check("scheme case accepted", p.request(["bearer " + token(1)], "list_bots"), True)
    check("owner lists bots", p.request(owner, "list_bots"), True)
    check("owner creates bot", p.request(owner, "create_bot"), True)
    check("owner deletes bot", p.request(owner, "delete_bot"), True)
    check("runtime cannot create bot", p.request(runtime, "create_bot"), False)
    check("runtime cannot delete bot", p.request(runtime, "delete_bot"), False)
    check("runtime reads assigned conversation", p.request(runtime, "conversation_read"), True)
    check("runtime operates assigned computer", p.request(runtime, "guest_exec"), True)
    check("cross-bot target denied", p.request(runtime, "guest_exec", bot="b"), False)
    check("cross-task target denied", p.request(runtime, "guest_exec", task="tb"), False)
    check("cross-conversation target denied", p.request(runtime, "conversation_read", conversation="cb"), False)
    check("assigned job readable", p.request(runtime, "job_read", job="ja"), True)
    check("cross-bot job denied", p.request(runtime, "job_read", job="jb"), False)
    check("unlisted tool denied", p.request(runtime, "host_shell"), False)
    check("unknown tool denied", p.request(runtime, "future_tool"), False)
    check("expiry boundary denied", p.authenticate(runtime, now=200), None)
    check("valid immediately before expiry", isinstance(p.authenticate(runtime, now=199), Grant), True)

    p.grants[digest(token(2))] = replace(Grant(), revoked=True)
    check("revoked grant denied", p.request(runtime, "guest_exec"), False)
    p = Policy()
    p.tasks["ta"] = ("a", "ca", 2, True)
    check("outgoing provider session denied after handoff", p.request(runtime, "guest_exec"), False)
    p = Policy()
    p.tasks["ta"] = ("a", "ca", 1, False)
    check("cancelled task denied", p.request(runtime, "guest_exec"), False)
    p = Policy()
    p.owner = digest(token(4))
    check("old owner token denied after rotation", p.request(owner, "list_bots"), False)
    check("new owner token accepted", p.request(["Bearer " + token(4)], "list_bots"), True)

    p = Policy()
    queued = p.prepare()
    p.stop_computer()
    check("stop precedes dispatch and prevents execution", p.dispatch(queued), False)
    check("stop prevents side effect", len(p.started), 0)
    check("conversation works while computer stopped", p.request(runtime, "conversation_read"), True)
    p.computer_stopped["a"] = False
    check("resume does not revive queued operation", p.dispatch(queued), False)
    check("new operation after explicit resume succeeds", p.dispatch(p.prepare()), True)

    p = Policy()
    queued = p.prepare()
    p.delete_bot()
    check("delete precedes dispatch and prevents execution", p.dispatch(queued), False)
    check("deleting bot cannot receive conversation work", p.request(runtime, "conversation_read"), False)
    check("delete prevents side effect", len(p.started), 0)

    p = Policy()
    check("dispatch can precede delete", p.dispatch(p.prepare()), True)
    p.delete_bot()
    check("delete cannot undo accepted operation", len(p.started), 1)
    check("delete blocks subsequent operation", p.dispatch(p.prepare()), False)
    p = Policy()
    p.grants.clear()
    check("API restart invalidates transient grants", p.request(runtime, "guest_exec"), False)

    here = pathlib.Path(__file__).resolve().parent
    with tempfile.TemporaryDirectory(prefix="elsebot-research-auth-") as scratch:
        binary = pathlib.Path(scratch) / "std_probe"
        subprocess.run(["rustc", "--edition=2021", str(here / "std_probe.rs"), "-o", str(binary)], check=True)
        subprocess.run([str(binary), scratch], check=True)
    print(f"PASS {passed} deterministic policy checks; Rust standard-library probe passed")


if __name__ == "__main__":
    main()
