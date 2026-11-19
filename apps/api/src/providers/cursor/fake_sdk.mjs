// A stand-in for @cursor/sdk in the Cursor adapter's tests. Every agent
// open and send is logged to `calls.jsonl` beside CURSOR_DATA_DIR. The
// message text picks the run's script: LIMIT fails on the usage limit,
// SHELL calls the shell, SLOW waits for a steer or a cancel.

import { appendFileSync, existsSync, mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { randomUUID } from "node:crypto";

const KEY = "test-key";

function log(entry) {
  const file = join(dirname(process.env.CURSOR_DATA_DIR), "calls.jsonl");
  appendFileSync(file, `${JSON.stringify(entry)}\n`);
}

function named(name, message, extra = {}) {
  return Object.assign(new Error(message), { name, ...extra });
}

function checkKey(apiKey) {
  if (apiKey !== KEY) {
    throw named("AuthenticationError", "invalid API key", { status: 401 });
  }
}

export class JsonlLocalAgentStore {
  constructor(dir) {
    this.dir = dir;
  }
}

export const Cursor = {
  models: {
    async list({ apiKey }) {
      checkKey(apiKey);
      return [
        {
          id: "composer-2",
          displayName: "Composer 2",
          parameters: [
            { id: "effort", values: [{ value: "low" }, { value: "high" }] },
            { id: "fast", values: [{ value: "false" }, { value: "true" }] },
          ],
        },
        { id: "plain", displayName: "Plain" },
      ];
    },
  },
};

class FakeRun {
  constructor(agentId, text) {
    this.id = `run-${randomUUID()}`;
    this.agentId = agentId;
    this.text = text;
    this.status = "running";
    this.wake = null;
    this.pending = [];
  }

  emit(message) {
    this.pending.push({ agent_id: this.agentId, run_id: this.id, ...message });
    this.wake?.();
  }

  say(text) {
    this.emit({ type: "assistant", message: { role: "assistant", content: [{ type: "text", text }] } });
  }

  async *stream() {
    yield { type: "status", status: "RUNNING", agent_id: this.agentId, run_id: this.id };
    if (this.text.includes("LIMIT")) {
      this.end("error", { message: "You've hit your usage limit", code: "plan_usage_limit" });
    } else if (this.text.includes("SHELL")) {
      this.emit({ type: "tool_call", call_id: "c1", name: "shell", status: "running", args: { command: "ls" } });
    } else if (this.text.includes("SLOW")) {
      this.say("working");
    } else {
      this.say("Hello");
      this.say(" there");
      this.emit({
        type: "usage",
        usage: { inputTokens: 10, outputTokens: 3, cacheReadTokens: 4, cacheWriteTokens: 0, totalTokens: 17 },
      });
      this.end("finished");
    }
    while (true) {
      while (this.pending.length) {
        yield this.pending.shift();
      }
      if (this.status !== "running") {
        return;
      }
      await new Promise((resolve) => {
        this.wake = resolve;
      });
    }
  }

  end(status, error) {
    this.status = status;
    this.error = error;
    this.wake?.();
  }

  async wait() {
    return { id: this.id, status: this.status, error: this.error };
  }

  async cancel() {
    log({ call: "cancel" });
    this.end("cancelled");
  }

  async steer(text) {
    log({ call: "steer", text });
    this.say(`steered: ${text}`);
    this.end("finished");
    return "complete_delivered";
  }
}

class FakeAgent {
  constructor(agentId) {
    this.agentId = agentId;
  }

  async send(message) {
    log({ call: "send", agentId: this.agentId, message });
    return new FakeRun(this.agentId, message.text);
  }
}

function open(call, agentId, options) {
  checkKey(options.apiKey);
  const { apiKey: _key, local, ...rest } = options;
  const { store, ...localRest } = local;
  log({ call, agentId, options: { ...rest, local: localRest }, store: store.dir });
  return new FakeAgent(agentId);
}

export const Agent = {
  async create(options) {
    const agentId = `agent-${randomUUID()}`;
    mkdirSync(options.local.store.dir, { recursive: true });
    writeFileSync(join(options.local.store.dir, agentId), "");
    return open("create", agentId, options);
  },
  async resume(agentId, options) {
    if (!existsSync(join(options.local.store.dir, agentId))) {
      throw named("AgentNotFoundError", `no agent ${agentId}`, { code: "agent_not_found" });
    }
    return open("resume", agentId, options);
  },
};
