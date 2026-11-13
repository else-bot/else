# Bot tool use

Replay small fake conversations against the instructions and MCP schemas in this checkout. The cases cover routine reruns, parked turns, email drafts, shared MCP discovery, secrets, and ordinary answers. The approach follows T3 Code's `evals/mcp-tool-use`, with no simulator model or Python dependencies.

`else-api context` constructs a main bot's owner request using `TurnInput`, with two other bots and one installed GitHub MCP. It exports `instructions`, `listed_tools`, `deferred_tools`, and `sizes`. Instructions come from `providers::prompt::instructions`; schemas come from the same builders as the production MCP endpoint. No database, server, login, or installation is needed.

The runner exports once per case and reuses that context for its samples. Claude sees the exported system instructions, the case's history and owner message, and its extra context blocks. History uses the text framing in `prompt::packet`. Registry and installed MCP blocks live in the user packet in production too, so cases supply those blocks explicitly when relevant. The representative registry and MCP in the Rust turn do not add text to the system instructions.

## Run

From the repository root, check all cases without calling a model:

```sh
python3 -I evals/bot-tool-use/run.py --self-test
python3 -I evals/bot-tool-use/run.py --context
python3 -I evals/bot-tool-use/run.py --dry-run --out evals/bot-tool-use/runs/dry
```

`--context` prints JSON lines with byte sizes only. `--dry-run` writes request artifacts and prints each exact shell invocation, working directory, stdin, and byte counts. Both paths avoid invoking `claude`. Neither requires a model login. The default exporter command is `cargo run --quiet --locked -p else-api --bin else-api -- context`. Rust must be installed.

To run paid samples, use an authenticated Claude CLI on `PATH`:

```sh
python3 -I evals/bot-tool-use/run.py --out evals/bot-tool-use/runs/baseline
python3 -I evals/bot-tool-use/run.py --case rerun-routine-now --samples 1 --out evals/bot-tool-use/runs/routine
python3 -I evals/bot-tool-use/run.py --baseline evals/bot-tool-use/runs/baseline/results.jsonl --stop-on-regression --out evals/bot-tool-use/runs/candidate
```

These commands spend model calls. The mock server executes no real tools. The runner uses `claude -p` with stream JSON input and output, the exported `--system-prompt`, `--strict-mcp-config`, empty `--setting-sources`, `--permission-mode dontAsk`, disabled hooks and slash commands, and medium effort, following `providers/claude.rs`. `--tools ""` removes host tools, and only `mcp__elsebot__*` is allowed. Each sample has a new session and a temporary working directory outside the checkout, removed after the sample. Model authentication uses the CLI's normal configuration.

Options:

| Flag | Meaning |
| --- | --- |
| `--case ID[,ID]` | Select exact case IDs. Repeatable. Default is every case. |
| `--samples N` | Samples per case, default 2. Runs sequentially. |
| `--model MODEL` | Default `claude-haiku-5-5`. |
| `--out DIR` | Empty output directory. Default is a timestamp under `runs/`. |
| `--dry-run` | Prepare one request per case and print its invocation, stdin, and sizes. No model calls. |
| `--context` | Print exported byte counts for each selected case, then exit. |
| `--baseline FILE` | Compare per-case pass rates with an earlier `results.jsonl`. |
| `--stop-on-regression` | Requires a baseline. Run cases with 100% baseline pass rates first. Stop on their first failed sample or execution error. |
| `--self-test` | Test the grader, packet, CLI configuration, stream parser, mock dispatch, and stdio transport offline. |
| `--api-bin PATH` | Use a built `else-api` instead of Cargo. Rebuild it after prompt or schema edits. |
| `--timeout SECONDS` | Wall time per model sample, default 180. The CLI also has a 12-turn limit. |

Live runs append each sample to `results.jsonl` before starting the next sample, then print per-case and overall pass rates. Rows contain checks, raw calls and results, final text, usage, model, sizes, and artifact paths. Execution errors are reported separately and excluded from pass rates; they still cause a nonzero exit. Any graded failure also exits nonzero. Each sample keeps `context.json`, `case.json`, `mcp.json`, `settings.json`, `stdin.jsonl`, `request.json`, `stream.jsonl`, `stderr.txt`, and `calls.jsonl`. Dry runs omit stream and call logs. `runs/` is ignored by Git.

## Add a case

Add one JSON object per line to `cases/*.ndjson`. Use a unique filesystem-safe `id`, a `kind` of `bad`, `guard`, or `control`, and an `experience` sentence explaining the mistake or behavior. `bad` reproduces a mistake, `guard` protects correct behavior, and `control` checks ordinary work. An optional `turn` of `delegated-task` exports `else-api context delegated-task` instead: Research working on a task the main bot delegated for an owner request, with no registry and the task as the latest message. `delegated-routine-task` is the same task delegated during a routine run. The default is `owner`.

`setup` contains:

- `prompt`: the latest owner message.
- `prior`: optional history objects with `kind`, `text`, and optional `status`, default `completed`. Use `user` or `assistant` for ordinary history.
- `context_blocks`: optional extra text blocks for the packet. The registry, saved-permission, and installed-MCP blocks a first owner turn carries are exported from Rust and always come first, so cases never copy their wording.
- `mocks`: optional ordered selectors with a `result`. The first matching mock wins. Unmatched tools return `{"status":"ok"}`. A mock with `"error": true` returns its result as a tool error, as production does for a refusal. Select inner tool names for deferred mocks, such as `routine_run`, while expectations select the actual `use_tool` wrapper.

For example, this mock parks delegation:

```json
{"name":"delegate","result":{"status":"pending","parked":true,"task_id":"20000000-0000-0000-0000-000000000001"}}
```

`expect` supports:

- `calls`: selectors that must each match at least one recorded call.
- `callsMustNot`: selectors that must never match.
- `maxCalls`: maximum number of calls, including `find_tool` and `use_tool` wrappers.
- `order`: selectors that must match distinct calls in order, allowing intervening calls.
- `finalTextMust` and `finalTextMustNot`: case-insensitive substrings of the final result text.
- Every sample also fails on a call to a tool name the CLI does not offer, such as a bare `delegate` instead of `mcp__elsebot__delegate`: it never reaches else.bot and costs a round trip.
- `maxFinalWords`: optional word limit for short acknowledgments.
- `errors`: selectors for calls whose error result is expected, such as a refusal a mock returns with `"error": true`. Every other call must succeed.

A selector is a tool name string or an object with `name`, optional `arguments`, and optional `absent`. Argument objects match partially and recursively. Dotted keys traverse nested objects. Strings beginning with `~` match a case-insensitive substring, or any element of an array; other values match exactly. Arrays match in order with equal length. `absent` lists argument paths that must be missing, which distinguishes `mcp_list` with no id from reading an MCP's skill.

```json
{"name":"use_tool","arguments":{"name":"routine_run","arguments.routine_id":"10000000-0000-0000-0000-000000000001"}}
```

```json
{"name":"mcp_request","arguments":{"method":"tools/list"},"absent":["params.names"]}
```

Every grade also rejects tool errors and, for parking tools, further calls after a result with top-level `parked: true`; a parked mock without `next` gets production's default `next`, so cases do not write their own guidance. A call to a withheld host tool (from the exported `host_tools`) is an execution error, not a graded failure, since production would have run it. The mock keeps serving after parking so the grader can detect that mistake. Discovery mirrors production's `tool_search::find`: a listed tool's name gets "call it directly", a query that names deferred tools exactly gets only those, and other queries use word scoring with at most five schemas. `use_tool` dispatches only deferred tools and rejects undeclared argument keys, like production's `tool_search::target`. Calls to unlisted tool names fail. All MCP `tools/call` attempts are logged once, including wrapper calls and errors. MCP initialization and `tools/list` transport messages are not counted as model tool calls.

The self-test feeds passing and failing fake logs through the grader and drives the stdio server with Python. Run it and both offline modes after changing cases or grading rules.

## Limits

This checks model choices under a fake owner request, not backend permissions, cards, credentials, remote servers, scheduling, or side effects. Mocks return static results and check only declared argument names and top-level string `maxLength`, not other JSON Schema constraints or server-side checks such as the rules prose limit. No native host tools, default skills, memory, attachments, compaction, or resumed CLI sessions are exercised. History is synthetic text, not a native session replay. Substring checks cannot detect every possible misleading chat response.

Byte counts describe the exported instructions and compact schema JSON. Dry runs also count the packet and stdin. `initial_context` adds instructions, listed schemas, and packet; deferred schemas enter only through discovery results. These counts exclude the CLI's own protocol text and model-specific schema serialization. They are not token estimates. Live result rows preserve the CLI's reported usage.

- The harness withholds Claude's native host shell and file tools so no case can touch this machine. Behavior that depends on them, such as asking `ask_permission` before installing host software or stopping after it returns pending, is guarded by unit tests on the instructions instead; `delegate-parks` covers stopping after a parked result.
