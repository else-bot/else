// else.bot's bridge to @cursor/sdk: newline-delimited JSON-RPC over stdio.
// One process serves one turn (or one model listing) and exits when its
// input closes. The API key arrives in `initialize`, never in the
// environment, so tools the agent runs cannot read it.
//
// Requests: initialize, models.list, agent.open, run.send, run.steer,
// run.cancel. Notifications: run/message (each SDK stream message, with
// tool arguments reduced to the MCP server and tool names) and run/result.

import { createInterface } from "node:readline";

const MAX_RESULT = 64 * 1024;
// Error codes the API reads; see cursor.rs.
const FAILED = -32000;
const REFUSED = -32001;
const SIGNED_OUT = -32002;
const USAGE_LIMIT = -32003;
const EFFORT_PARAMS = ["effort", "reasoning"];
const USAGE_CODE =
  /^(?:resource_exhausted|rate_limited(?:_changeable)?|\w*_usage_limit|\w*_rate_limit(?:_exceeded)?)$/i;

let sdk;
let apiKey;
let agent;
let run;

// The SDK leaves some promises unhandled (shell spawn failures, writes to
// closed streams, stall aborts). Report them and keep serving the turn; a
// run they wedge ends at else.bot's turn deadline.
process.on("unhandledRejection", (reason) => {
  process.stderr.write(`unhandled rejection: ${describe(reason)}\n`);
});

function describe(error) {
  return error instanceof Error ? `${error.name}: ${error.message}` : String(error);
}

function write(message) {
  process.stdout.write(`${JSON.stringify(message)}\n`);
}

function notify(method, params) {
  write({ method, params });
}

function usageLimited(error) {
  return (
    error?.name === "RateLimitError" ||
    error?.status === 429 ||
    (typeof error?.code === "string" && USAGE_CODE.test(error.code))
  );
}

function signedOut(error) {
  return (
    error?.name === "AuthenticationError" ||
    error?.status === 401 ||
    error?.code === "unauthenticated"
  );
}

function kindOf(error) {
  if (usageLimited(error)) {
    return "usage_limit";
  }
  return signedOut(error) ? "signed_out" : null;
}

function failure(message, code) {
  return Object.assign(new Error(message), { rpcCode: code });
}

// A tool call without its arguments, except the MCP server and tool names,
// and with its result only while small: the API reads results only to see
// whether an else.bot tool parked.
function bounded(message) {
  if (message?.type !== "tool_call") {
    return message;
  }
  const { args, result, ...rest } = message;
  const kept = { ...rest };
  if (args && typeof args === "object") {
    kept.args = { providerIdentifier: args.providerIdentifier, toolName: args.toolName };
  }
  if (result !== undefined) {
    const text = JSON.stringify(result);
    if (text !== undefined && text.length <= MAX_RESULT) {
      kept.result = result;
    } else {
      kept.result_truncated = true;
    }
  }
  return kept;
}

async function load() {
  sdk ??= await import("@cursor/sdk");
  return sdk;
}

// else.bot's { id, effort, fast } as the model's own parameters.
async function resolveModel(selection) {
  const { Cursor } = await load();
  const models = await Cursor.models.list({ apiKey });
  const model = models.find((m) => m.id === selection.id);
  if (!model) {
    throw failure(`this Cursor account has no model ${selection.id}`, REFUSED);
  }
  const parameters = model.parameters ?? [];
  const takes = (parameter, value) => parameter?.values?.some((v) => v.value === value);
  const params = [];
  if (selection.effort) {
    const effort = parameters.find((p) => EFFORT_PARAMS.includes(p.id));
    if (!takes(effort, selection.effort)) {
      throw failure(`${model.id} takes no ${selection.effort} effort`, REFUSED);
    }
    params.push({ id: effort.id, value: selection.effort });
  }
  const fast = parameters.find((p) => p.id === "fast");
  if (selection.fast && !takes(fast, "true")) {
    throw failure(`${model.id} has no Fast mode`, REFUSED);
  }
  if (fast) {
    params.push({ id: "fast", value: selection.fast ? "true" : "false" });
  }
  return { id: model.id, params };
}

async function follow(current) {
  try {
    for await (const message of current.stream()) {
      notify("run/message", { message: bounded(message) });
    }
    const result = await current.wait();
    notify("run/result", {
      status: result.status,
      kind: result.error ? kindOf(result.error) : null,
      error: result.error ?? null,
    });
  } catch (error) {
    notify("run/result", {
      status: "error",
      kind: kindOf(error),
      error: { message: describe(error), code: error?.code ?? null },
    });
  }
}

const methods = {
  async initialize(params) {
    apiKey = params.apiKey;
    await load();
    return {};
  },

  async "models.list"() {
    const { Cursor } = await load();
    return { models: await Cursor.models.list({ apiKey }) };
  },

  // Resume the recorded agent, or create one when the store no longer
  // has it. The tool restrictions are not persisted, so every open sends
  // them again.
  async "agent.open"(params) {
    const { Agent, JsonlLocalAgentStore } = await load();
    const store = new JsonlLocalAgentStore(params.storeDir);
    const options = {
      ...params.options,
      apiKey,
      model: await resolveModel(params.options.model),
      local: { ...params.options.local, store },
    };
    let resumed = false;
    if (params.agentId) {
      try {
        agent = await Agent.resume(params.agentId, options);
        resumed = true;
      } catch (error) {
        if (error?.name !== "AgentNotFoundError" && error?.name !== "UnknownAgentError") {
          throw error;
        }
        process.stderr.write(`agent ${params.agentId} is gone; starting a new one\n`);
      }
    }
    agent ??= await Agent.create(options);
    return { agentId: agent.agentId, resumed };
  },

  async "run.send"(params) {
    if (!agent || run) {
      throw failure("no agent is open, or a run already started", REFUSED);
    }
    run = await agent.send(params.message);
    follow(run);
    return { runId: run.id };
  },

  async "run.steer"(params) {
    if (!run || run.status !== "running") {
      throw failure("no running turn to steer", REFUSED);
    }
    if (typeof run.steer !== "function") {
      throw failure("this run takes no steering", REFUSED);
    }
    return { outcome: await run.steer(params.text) };
  },

  async "run.cancel"() {
    if (run && run.status === "running") {
      await run.cancel();
    }
    return {};
  },
};

async function handle(request) {
  if (request.id === undefined) {
    return;
  }
  const method = methods[request.method];
  if (!method) {
    write({ id: request.id, error: { code: -32601, message: `unknown method ${request.method}` } });
    return;
  }
  try {
    write({ id: request.id, result: await method(request.params ?? {}) });
  } catch (error) {
    const kind = kindOf(error);
    const code =
      error?.rpcCode ??
      (kind === "usage_limit" ? USAGE_LIMIT : kind === "signed_out" ? SIGNED_OUT : FAILED);
    write({ id: request.id, error: { code, message: describe(error) } });
  }
}

const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
lines.on("line", (line) => {
  if (!line.trim()) {
    return;
  }
  let request;
  try {
    request = JSON.parse(line);
  } catch {
    process.stderr.write("ignored a malformed request\n");
    return;
  }
  handle(request);
});
lines.on("close", async () => {
  if (run && run.status === "running") {
    await run.cancel().catch(() => {});
  }
  process.exit(0);
});
