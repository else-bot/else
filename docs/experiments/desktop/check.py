#!/usr/bin/env python3
"""Offline examples of the proposed desktop contract, not an API implementation."""

import copy
import json
import sys
import unittest
from pathlib import Path


class Conflict(Exception):
    pass


class Gone(Exception):
    pass


class Reset(Exception):
    pass


class Model:
    def __init__(self):
        self.bots = {}
        self.creations = {}
        self.default_bot_id = None
        self.initialized = False
        self.server_instance_id = "boot-1"

    def setup(self):
        if not self.initialized:
            self.default_bot_id = self.create("setup", "Main", "Help the owner")
            self.initialized = True

    def startup(self):
        # Startup observes saved state. Installation setup is a separate action.
        return self.default_bot_id

    def create(self, request, name, role):
        payload = (name, role)
        if request in self.creations:
            previous, bot_id = self.creations[request]
            if payload != previous:
                raise Conflict("request_reused")
            return bot_id
        bot_id = f"bot-{len(self.bots) + 1}"
        self.bots[bot_id] = {
            "name": name, "role": role, "deleted": False, "cleanup": None,
            "seq": 0, "events": [], "messages": {}, "turns": {},
            "jobs": {}, "generation": 0, "grants": set(),
            "memory": ["retained memory"], "files": ["retained file"],
            "conversation": f"conversation-{bot_id}", "provider": "p1",
            "cancellations": {}, "stopping": False,
        }
        self.creations[request] = (payload, bot_id)
        return bot_id

    def event(self, bot, kind, data):
        bot["seq"] += 1
        bot["events"].append({"seq": bot["seq"], "kind": kind, "data": data})

    def send(self, bot_id, request, text, provider):
        bot = self.bots[bot_id]
        if bot["deleted"]:
            raise Gone("bot_deleted")
        payload = (text, provider)
        if request in bot["turns"]:
            turn = bot["turns"][request]
            if payload != turn["payload"]:
                raise Conflict("request_reused")
            return turn
        if bot["stopping"]:
            raise Conflict("cancellation_pending")
        turn = {"payload": payload, "state": "queued", "provider": provider}
        bot["turns"][request] = turn
        bot["provider"] = provider
        bot["messages"][request] = {"id": request, "text": text, "role": "user"}
        self.event(bot, "item.upsert", copy.deepcopy(bot["messages"][request]))
        return turn

    def begin_reply(self, bot_id, item_id):
        bot = self.bots[bot_id]
        bot["messages"][item_id] = {"id": item_id, "text": "", "role": "assistant"}
        self.event(bot, "item.upsert", copy.deepcopy(bot["messages"][item_id]))

    def append(self, bot_id, item_id, text, generation):
        bot = self.bots[bot_id]
        if bot["deleted"] or bot["stopping"] or generation != bot["generation"]:
            return False
        message = bot["messages"][item_id]
        offset = len(message["text"].encode("utf-8"))
        message["text"] += text
        self.event(bot, "item.delta", {"item_id": item_id, "offset_utf8": offset, "text": text})
        return True

    def snapshot(self, bot_id):
        bot = self.bots[bot_id]
        if bot["deleted"]:
            raise Gone("bot_deleted")
        return copy.deepcopy({"cursor": bot["seq"], "messages": bot["messages"],
                              "server_instance_id": self.server_instance_id})

    def replay(self, bot_id, cursor, server_instance_id=None):
        bot = self.bots[bot_id]
        if bot["deleted"]:
            raise Gone("bot_deleted")
        if server_instance_id is not None and server_instance_id != self.server_instance_id:
            raise Reset("server_restarted")
        if cursor < 0 or cursor > bot["seq"]:
            raise Reset("cursor_invalid")
        return copy.deepcopy([event for event in bot["events"] if event["seq"] > cursor])

    def cancel(self, bot_id, request):
        bot = self.bots[bot_id]
        if bot["deleted"]:
            raise Gone("bot_deleted")
        if request in bot["cancellations"]:
            return bot["cancellations"][request]
        bot["generation"] += 1
        bot["grants"].clear()
        bot["stopping"] = True
        for turn in bot["turns"].values():
            if turn["state"] not in {"completed", "cancelled"}:
                turn["state"] = "cancelling"
        for job in bot["jobs"].values():
            if job["state"] != "completed":
                job["state"] = "cancelling"
        receipt = {"request_id": request, "state": "pending"}
        bot["cancellations"][request] = receipt
        return receipt

    def finish_cancel(self, bot_id, request):
        bot = self.bots[bot_id]
        for group in (bot["turns"], bot["jobs"]):
            for entry in group.values():
                if entry["state"] == "cancelling":
                    entry["state"] = "cancelled"
        bot["stopping"] = False
        bot["cancellations"][request]["state"] = "done"

    def delete(self, bot_id):
        bot = self.bots[bot_id]
        if bot["deleted"]:
            return bot["cleanup"]
        self.cancel(bot_id, "delete")
        bot["deleted"] = True
        bot["cleanup"] = "pending"
        if self.default_bot_id == bot_id:
            self.default_bot_id = None
        return bot["cleanup"]


def apply_event(view, event):
    if event["seq"] <= view["cursor"]:
        return
    if event["seq"] != view["cursor"] + 1:
        raise Reset("event_gap")
    data = event["data"]
    if event["kind"] == "item.upsert":
        view["messages"][data["id"]] = copy.deepcopy(data)
    elif event["kind"] == "item.delta":
        message = view["messages"][data["item_id"]]
        if len(message["text"].encode("utf-8")) != data["offset_utf8"]:
            raise Reset("offset_mismatch")
        message["text"] += data["text"]
    else:
        raise Reset("unsupported_event")
    view["cursor"] = event["seq"]


def parse_fixture_frames(chunks):
    """Parse only LF-terminated frames generated by this fixture, not general SSE."""
    pending = b""
    events = []
    for chunk in chunks:
        pending += chunk
        while b"\n\n" in pending:
            raw, pending = pending.split(b"\n\n", 1)
            fields = {}
            for line in raw.decode("utf-8").splitlines():
                if line and not line.startswith(":"):
                    key, value = line.split(":", 1)
                    fields[key] = value.lstrip(" ")
            if "data" in fields:
                events.append((int(fields["id"]), json.loads(fields["data"])))
    return events, pending


class ContractCases(unittest.TestCase):
    def setUp(self):
        self.model = Model()
        self.model.setup()
        self.bot_id = self.model.default_bot_id

    def test_create_retry_and_payload_conflict(self):
        new = self.model.create("create-1", "Work", "Work tasks")
        self.assertEqual(new, self.model.create("create-1", "Work", "Work tasks"))
        with self.assertRaises(Conflict):
            self.model.create("create-1", "Changed", "Work tasks")
        self.assertEqual(len(self.model.bots), 2)

    def test_send_retry_keeps_one_bubble_and_bound_provider(self):
        first = self.model.send(self.bot_id, "send-1", "Hello", "p1")
        self.assertIs(first, self.model.send(self.bot_id, "send-1", "Hello", "p1"))
        with self.assertRaises(Conflict):
            self.model.send(self.bot_id, "send-1", "Hello", "p2")
        self.model.send(self.bot_id, "send-2", "Next", "p2")
        self.assertEqual(first["provider"], "p1")
        self.assertEqual(len(self.model.bots[self.bot_id]["messages"]), 2)

    def test_snapshot_race_and_duplicate_replay(self):
        self.model.begin_reply(self.bot_id, "reply")
        view = self.model.snapshot(self.bot_id)
        # Commit between snapshot and stream connection must be replayed.
        self.model.append(self.bot_id, "reply", "Hello", 0)
        events = self.model.replay(self.bot_id, view["cursor"])
        for event in events + events:
            apply_event(view, event)
        self.assertEqual(view, self.model.snapshot(self.bot_id))

    def test_unicode_offsets_gap_and_offset_recovery(self):
        self.model.begin_reply(self.bot_id, "reply")
        view = self.model.snapshot(self.bot_id)
        self.model.append(self.bot_id, "reply", "é", 0)
        self.model.append(self.bot_id, "reply", "🙂", 0)
        events = self.model.replay(self.bot_id, view["cursor"])
        self.assertEqual(events[1]["data"]["offset_utf8"], 2)
        with self.assertRaises(Reset):
            apply_event(copy.deepcopy(view), events[1])
        for event in events:
            apply_event(view, event)
        self.assertEqual(view["messages"]["reply"]["text"], "é🙂")
        broken = copy.deepcopy(view)
        broken["cursor"] -= 1
        broken["messages"]["reply"]["text"] = ""
        with self.assertRaises(Reset):
            apply_event(broken, events[1])

    def test_sse_bytes_fragmented_at_every_boundary(self):
        payload = {"id": "reply", "text": "é🙂\nnext line"}
        wire = (": keepalive\n\nid: 8\nevent: item.delta\ndata: " +
                json.dumps(payload, ensure_ascii=False) + "\n\n").encode("utf-8")
        for split in range(len(wire) + 1):
            with self.subTest(split=split):
                events, pending = parse_fixture_frames([wire[:split], wire[split:]])
                self.assertEqual(events, [(8, payload)])
                self.assertEqual(pending, b"")
        self.assertEqual(parse_fixture_frames([bytes([b]) for b in wire])[0], [(8, payload)])

    def test_partial_sse_frame_does_not_advance_cursor(self):
        self.assertEqual(parse_fixture_frames([b"id: 9\ndata: {}\n"])[0], [])

    def test_future_cursor_requires_snapshot(self):
        with self.assertRaises(Reset):
            self.model.replay(self.bot_id, 100)

    def test_restart_requires_snapshot_even_if_sequences_match(self):
        view = self.model.snapshot(self.bot_id)
        self.model.server_instance_id = "boot-2"
        with self.assertRaises(Reset):
            self.model.replay(self.bot_id, view["cursor"], view["server_instance_id"])
        self.assertEqual(self.model.snapshot(self.bot_id)["cursor"], view["cursor"])

    def test_selection_does_not_cancel_other_bot_work(self):
        other = self.model.create("other", "Other", "Other tasks")
        turn = self.model.send(other, "other-message", "Continue", "p1")
        self.model.snapshot(self.bot_id)
        self.assertEqual(turn["state"], "queued")

    def test_cancel_all_work_blocks_new_send_and_retries_are_safe(self):
        bot = self.model.bots[self.bot_id]
        bot["jobs"]["background"] = {"state": "running"}
        bot["grants"].add("fake-task-grant")
        self.model.send(self.bot_id, "a", "Work", "p1")
        receipt = self.model.cancel(self.bot_id, "stop-1")
        self.assertEqual(receipt["state"], "pending")
        self.assertEqual(bot["jobs"]["background"]["state"], "cancelling")
        self.assertEqual(bot["grants"], set())
        with self.assertRaises(Conflict):
            self.model.send(self.bot_id, "b", "Too soon", "p1")
        self.model.finish_cancel(self.bot_id, "stop-1")
        new = self.model.send(self.bot_id, "c", "New work", "p2")
        self.model.cancel(self.bot_id, "stop-1")
        self.assertEqual(new["state"], "queued")
        self.assertFalse(bot["stopping"])

    def test_switch_keeps_conversation_and_rejects_stale_output(self):
        bot = self.model.bots[self.bot_id]
        conversation = bot["conversation"]
        self.model.begin_reply(self.bot_id, "old")
        self.model.cancel(self.bot_id, "switch-stop")
        self.model.finish_cancel(self.bot_id, "switch-stop")
        self.model.send(self.bot_id, "followup", "Continue", "p2")
        self.assertFalse(self.model.append(self.bot_id, "old", "late text", 0))
        self.assertEqual(bot["conversation"], conversation)

    def test_delete_revokes_retains_and_never_recreates_main(self):
        bot = self.model.bots[self.bot_id]
        self.model.send(self.bot_id, "a", "Keep history", "p1")
        self.model.begin_reply(self.bot_id, "reply")
        bot["grants"].add("fake-grant")
        retained = copy.deepcopy({key: bot[key] for key in ("messages", "memory", "files")})
        self.assertEqual(self.model.delete(self.bot_id), "pending")
        generation = bot["generation"]
        self.assertEqual(self.model.delete(self.bot_id), "pending")
        self.assertEqual(bot["generation"], generation)
        self.assertEqual(bot["grants"], set())
        for _ in range(3):
            self.assertIsNone(self.model.startup())
            self.model.setup()
        self.assertEqual(len(self.model.bots), 1)
        self.assertFalse(self.model.append(self.bot_id, "reply", "late", generation))
        self.assertEqual(retained, {key: bot[key] for key in retained})
        with self.assertRaises(Gone):
            self.model.snapshot(self.bot_id)
        with self.assertRaises(Gone):
            self.model.send(self.bot_id, "b", "No resurrection", "p1")


if __name__ == "__main__":
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(ContractCases)
    result = unittest.TextTestRunner(verbosity=2, stream=sys.stdout).run(suite)
    summary = {
        "status": "PASS" if result.wasSuccessful() else "FAIL",
        "cases": result.testsRun,
        "failures": len(result.failures),
        "errors": len(result.errors),
        "scope": "Synthetic sequential contract model and generated SSE byte frames only",
        "not_verified": ["Rust API", "GPUI", "PostgreSQL transactions", "HTTP transport",
                         "CLI cancellation", "VM shutdown", "native session handoff"],
    }
    Path(__file__).with_name("results.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary))
    sys.exit(not result.wasSuccessful())
