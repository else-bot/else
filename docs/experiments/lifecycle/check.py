#!/usr/bin/env python3
"""Deterministic lifecycle model. No subprocesses, database, network, or VMs."""

from copy import deepcopy
from dataclasses import asdict, dataclass, field
from hashlib import sha256
from itertools import permutations
import json
from pathlib import Path


@dataclass
class Model:
    bot_status: str = "active"
    computer_desired: str = "on_demand"
    computer_generation: int = 0
    phase: str = "running"
    actual: str = "running"
    generation: int = 0
    charged: bool = True
    leases: dict = field(default_factory=dict)
    lease_generations: dict = field(default_factory=dict)
    retained_data: list = field(default_factory=lambda: ["chats", "memory", "vm_disk", "sessions"])
    job_phase: str = "none"
    job_outcome: str | None = None
    dispatches: int = 0
    guest_live: bool = False
    native_id_saved: bool = False
    turn: str = "running"

    def acquire(self, key):
        if key in self.leases:
            return self.leases[key]
        if self.bot_status != "active" or self.computer_desired != "on_demand":
            return "rejected"
        self.leases[key] = "held" if self.phase == "running" else "waiting"
        self.lease_generations[key] = self.computer_generation
        return self.leases[key]

    def can_host_turn(self):
        return self.bot_status == "active"

    def release(self, key):
        if key == "job" and self.job_phase in {"dispatching", "running", "checking"}:
            return
        if key in self.leases:
            self.leases[key] = "released"

    def idle(self):
        if self.phase == "running" and not self.holders() and not self.waiters():
            self.phase = "stopping"
            self.generation += 1

    def holders(self):
        return [k for k, v in self.leases.items() if v == "held"]

    def waiters(self):
        return [k for k, v in self.leases.items() if v == "waiting"]

    def stop_ack(self):
        if self.phase != "stopping":
            return
        assert not self.guest_live
        self.actual = "stopped"
        self.phase = "stopped"
        self.charged = False

    def start(self):
        if (self.bot_status == "active" and self.computer_desired == "on_demand"
                and self.phase == "stopped" and self.waiters()):
            self.phase = "starting"
            self.generation += 1
            self.charged = True

    def start_ack(self):
        if self.phase != "starting":
            return
        self.actual = "running"
        if self.bot_status == "active" and self.computer_desired == "on_demand":
            self.phase = "running"
            for key in self.waiters():
                if self.lease_generations[key] == self.computer_generation:
                    self.leases[key] = "held"
                else:
                    self.leases[key] = "released"
        else:
            self.phase = "stopping"
            self.generation += 1

    def delete(self):
        self.bot_status = "deleting"
        self.stop_computer()

    def stop_computer(self):
        self.computer_desired = "stopped"
        self.computer_generation += 1
        for key in self.leases:
            if key != "job" or not self.guest_live:
                self.leases[key] = "released"
        if self.phase == "running" and not self.guest_live:
            self.phase = "stopping"
            self.generation += 1
        # An in-flight start must settle before shutdown can complete.

    def resume_computer(self):
        if self.bot_status == "active":
            self.computer_desired = "on_demand"
            self.computer_generation += 1

    def complete_delete(self):
        if self.bot_status == "deleting" and self.phase == "stopped":
            self.bot_status = "deleted"

    def dispatch_intent(self):
        assert self.job_phase == "none"
        assert self.acquire("job") == "held"
        self.job_phase = "dispatching"

    def runtime_accepts(self):
        assert self.job_phase == "dispatching"
        self.dispatches += 1
        self.guest_live = True

    def save_ack(self):
        assert self.guest_live
        self.native_id_saved = True
        self.job_phase = "running"

    def finish_job(self):
        self.guest_live = False
        self.job_phase = "finished"
        self.job_outcome = "succeeded"
        self.release("job")

    def restart(self):
        # JSON round-trip represents durable intent. Actual runtime facts are
        # the fake runtime's independent observation, never inferred from expiry.
        result = Model(**json.loads(json.dumps(asdict(self))))
        if result.job_phase in {"dispatching", "running", "checking"}:
            if result.guest_live and result.native_id_saved:
                result.job_phase = "running"
            elif result.guest_live:
                result.job_phase = "checking"
            else:
                result.job_phase = "finished"
                result.job_outcome = "unknown"
                result.release("job")
        result.check()
        return result

    def check(self):
        assert not self.holders() or self.phase == "running"
        assert not self.guest_live or "job" in self.holders()
        assert self.actual != "running" or self.charged
        assert self.phase not in {"starting", "stopping", "paused"} or self.charged
        assert self.bot_status != "deleted" or self.actual == "stopped"
        assert all(self.lease_generations[k] == self.computer_generation
                   for k in self.holders() if k != "job")
        assert self.dispatches <= 1


def idle_races():
    checked = 0
    for order in permutations(("acquire", "idle", "ack")):
        if order.index("ack") < order.index("idle"):
            continue
        for crash_after in range(4):
            model = Model()
            for index, event in enumerate(order):
                if index == crash_after:
                    model = model.restart()
                if event == "acquire":
                    model.acquire("request-1")
                elif event == "idle":
                    model.idle()
                else:
                    model.stop_ack()
                model.check()
            if crash_after == 3:
                model = model.restart()
            model.start()
            model.start_ack()
            model.check()
            assert model.leases == {"request-1": "held"}
            assert model.actual == "running"
            checked += 1
    return checked


def deletion_races():
    checked = 0
    for order in permutations(("start", "start_ack", "delete")):
        if order.index("start_ack") < order.index("start"):
            continue
        for crash_after in range(4):
            model = Model(phase="stopped", actual="stopped", charged=False)
            retained = deepcopy(model.retained_data)
            model.acquire("request-1")
            for index, event in enumerate(order):
                if index == crash_after:
                    model = model.restart()
                getattr(model, event)()
                model.check()
            if crash_after == 3:
                model = model.restart()
            model.stop_ack()
            model.complete_delete()
            model = model.restart()
            assert model.bot_status == "deleted"
            assert model.actual == "stopped" and model.retained_data == retained
            assert not model.can_host_turn()
            assert model.acquire("late-request") == "rejected"
            checked += 1
    return checked


def dispatch_crashes():
    checked = 0
    for crash_after in range(4):
        model = Model()
        steps = ("dispatch_intent", "runtime_accepts", "save_ack")
        for event in steps[:crash_after]:
            getattr(model, event)()
        model = model.restart()
        # The dispatcher never resends an uncertain command, even when this
        # fake runtime knows the command was never accepted.
        before = model.dispatches
        model = model.restart()
        model.idle()
        model.check()
        assert model.dispatches == before
        if model.guest_live:
            assert model.phase == "running" and model.holders() == ["job"]
        checked += 1
    return checked


def edge_cases():
    results = {}
    model = Model()
    assert model.acquire("same-key") == model.acquire("same-key") == "held"
    model.release("same-key")
    model.release("same-key")
    assert model.acquire("same-key") == "released"
    results["retry_release_never_reacquires"] = True

    model = Model()
    model.dispatch_intent()
    model.runtime_accepts()
    model.save_ack()
    model.turn = "finished"
    model.release("job")
    model = model.restart()  # Expired lease triggers this observation.
    model.idle()
    assert model.guest_live and model.holders() == ["job"]
    model.finish_job()
    model.idle()
    model.stop_ack()
    model.check()
    results["background_job_outlives_turn_and_expiry"] = True

    model = Model()
    model.idle()
    model = model.restart()  # Stop wait timed out; no stop acknowledgment.
    assert model.phase == "stopping" and model.charged
    results["stop_timeout_retains_reservation"] = True

    model = Model(phase="paused", actual="paused")
    model.check()
    assert model.charged
    results["pause_retains_reservation"] = True

    model = Model(computer_desired="stopped", phase="stopped", actual="stopped", charged=False)
    for _ in range(3):
        model = model.restart()
    assert model.acquire("owner-stopped") == "rejected" and not model.charged
    assert model.can_host_turn() and model.phase == "stopped"
    results["stopped_computer_allows_host_turn_without_waking"] = True

    model = Model(phase="stopped", actual="stopped", charged=False)
    model.acquire("before-stop")
    model.stop_computer()
    model.resume_computer()
    model.start()
    assert model.phase == "stopped" and model.acquire("before-stop") == "released"
    assert model.acquire("after-resume") == "waiting"
    model.start()
    model.start_ack()
    model.check()
    assert model.leases["after-resume"] == "held"
    results["explicit_resume_does_not_revive_old_queued_work"] = True

    model = Model()
    retained = deepcopy(model.retained_data)
    model.delete()
    assert not model.can_host_turn() and model.acquire("deleted") == "rejected"
    model.stop_ack()
    model.complete_delete()
    model = model.restart()
    assert model.bot_status == "deleted" and model.actual == "stopped"
    assert model.retained_data == retained
    results["soft_delete_retains_chats_memory_disk_sessions"] = True

    model = Model()
    model.dispatch_intent()
    model.runtime_accepts()
    model.save_ack()
    broken = deepcopy(model)
    broken.leases["job"] = "released"  # The naive expiry policy.
    try:
        broken.check()
    except AssertionError:
        results["negative_control_expiry_detected"] = True
    else:
        raise AssertionError("Expiry negative control escaped")

    broken = Model()
    broken.idle()
    broken.charged = False  # The naive stop-timeout policy.
    try:
        broken.check()
    except AssertionError:
        results["negative_control_timeout_detected"] = True
    else:
        raise AssertionError("Timeout negative control escaped")
    return results


def main():
    result = {
        "status": "PASS",
        "evidence": "synthetic sequential interleavings with JSON restart snapshots",
        "idle_acquire_restart_cases": idle_races(),
        "delete_start_restart_cases": deletion_races(),
        "dispatch_crash_cases": dispatch_crashes(),
        "edge_cases": edge_cases(),
        "model_sha256": sha256(Path(__file__).read_bytes()).hexdigest(),
        "limits": [
            "No PostgreSQL concurrency or transaction behavior tested",
            "No real runtime, CLI, process containment, clock, or resource measurement",
            "Runtime observation is a perfect fake and dispatch is never retried",
            "Restart JSON contains fake runtime facts as well as database intent",
        ],
    }
    print(json.dumps(result, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
