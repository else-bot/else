#!/usr/bin/env python3
"""Fixture-only elsebot MCP server. No tool executes host or network actions."""

import argparse
import json
from pathlib import Path
import re
import sys


MISSING = object()


def at(value, path):
    for part in path.split("."):
        if not isinstance(value, dict) or part not in value:
            return MISSING
        value = value[part]
    return value


def partial(actual, expected):
    if isinstance(expected, str) and expected.startswith("~"):
        if isinstance(actual, list):
            return any(partial(item, expected) for item in actual)
        return isinstance(actual, str) and expected[1:].casefold() in actual.casefold()
    if isinstance(expected, dict):
        return isinstance(actual, dict) and all(
            partial(at(actual, key), value) for key, value in expected.items()
        )
    if isinstance(expected, list):
        return isinstance(actual, list) and len(actual) == len(expected) and all(
            partial(a, e) for a, e in zip(actual, expected)
        )
    return actual is not MISSING and type(actual) is type(expected) and actual == expected


def matches(call, selector):
    if isinstance(selector, str):
        selector = {"name": selector}
    return (
        partial(call, {k: v for k, v in selector.items() if k != "absent"})
        and all(at(call.get("arguments", {}), key) is MISSING
                for key in selector.get("absent", []))
    )


def find(deferred, query, listed=()):
    query = query.lower()
    bare = query.strip().removeprefix("mcp__elsebot__")
    if bare in listed:
        # Mirrors tool_search::find for a tool that is already listed.
        return {"tools": [], "listed": f"{bare} is in your tool list, as mcp__elsebot__{bare} where "
                                       "your tools carry that prefix: call it directly."}
    words = [w for w in re.split(r"[\W_]+", query) if len(w) > 2]
    scored = []
    for tool in deferred:
        name = tool["name"]
        score = 100 if name in query else sum(
            3 if any(p.startswith(w) or w.startswith(p) for p in name.split("_"))
            else int(w in tool.get("description", "").lower()) for w in words
        )
        if score:
            scored.append((score, tool))
    tools = [t for _, t in sorted(scored, key=lambda pair: -pair[0])[:5]]
    return {"tools": tools} if tools else {
        "tools": [], "available": [t["name"] for t in deferred]
    }


def too_long(schema, arguments):
    """Names the first string argument over its schema maxLength, as production rejects it."""
    for key, prop in schema.get("inputSchema", {}).get("properties", {}).items():
        value, limit = arguments.get(key), prop.get("maxLength")
        if isinstance(value, str) and limit is not None and len(value) > limit:
            return key
    return None


def tool_result(value, error=False):
    result = {"content": [{"type": "text", "text": json.dumps(value, ensure_ascii=False)}]}
    if error:
        result["isError"] = True
    return result


class Server:
    def __init__(self, context, case, log):
        self.context = context
        self.mocks = case["setup"].get("mocks", [])
        self.log = log
        self.listed = {t["name"]: t for t in context["listed_tools"]}
        self.deferred = {t["name"]: t for t in context["deferred_tools"]}

    def dispatch(self, name, arguments):
        if name not in self.listed:
            return tool_result("Tool is not listed; discover it with find_tool.", True)
        if not isinstance(arguments, dict):
            return tool_result("Give arguments as an object.", True)
        if name == "find_tool":
            query = arguments.get("query")
            if not isinstance(query, str) or not query.strip() or len(query.encode()) > 200:
                return tool_result("Give a query of at most 200 bytes.", True)
            return tool_result(find(self.context["deferred_tools"], query, self.listed))
        if name == "use_tool":
            name, arguments = arguments.get("name"), arguments.get("arguments")
            schema = self.deferred.get(name) if isinstance(name, str) else None
            if schema is None or not isinstance(arguments, dict):
                return tool_result("Name a deferred tool and give its arguments as an object.", True)
            if arguments.keys() - schema["inputSchema"].get("properties", {}).keys():
                return tool_result("Use only arguments declared by find_tool's schema.", True)
        schema = self.deferred.get(name) or self.listed.get(name, {})
        if key := too_long(schema, arguments):
            return tool_result(f"{key} exceeds maxLength {schema['inputSchema']['properties'][key]['maxLength']}.", True)
        call = {"name": name, "arguments": arguments}
        for mock in self.mocks:
            if matches(call, {k: v for k, v in mock.items() if k not in ("result", "error")}):
                result = dict(mock["result"]) if isinstance(mock["result"], dict) else mock["result"]
                # Production's with_next: a parked result without guidance gets the default.
                if (isinstance(result, dict) and result.get("parked") is True and "next" not in result
                        and name in self.context.get("parking_tools", ())):
                    result["next"] = self.context.get("parked_next", "")
                return tool_result(result, mock.get("error", False))
        return tool_result({"status": "ok"})

    def handle(self, message):
        method = message.get("method")
        params = message.get("params", {})
        if method == "initialize":
            return {"protocolVersion": params.get("protocolVersion", "2024-11-05"),
                    "capabilities": {"tools": {"listChanged": False}},
                    "serverInfo": {"name": "elsebot", "version": "eval"},
                    "instructions": self.context.get("server_instructions", "")}
        if method == "ping":
            return {}
        if method == "tools/list":
            return {"tools": self.context["listed_tools"]}
        if method == "tools/call":
            name, arguments = params.get("name"), params.get("arguments", {})
            result = self.dispatch(name, arguments)
            with self.log.open("a", encoding="utf-8") as stream:
                stream.write(json.dumps({"name": name, "arguments": arguments,
                                         "result": result}, ensure_ascii=False) + "\n")
            return result
        raise ValueError(f"Unsupported method: {method}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--context", type=Path, required=True)
    parser.add_argument("--case", type=Path, required=True)
    parser.add_argument("--log", type=Path, required=True)
    args = parser.parse_args()
    server = Server(json.loads(args.context.read_text()), json.loads(args.case.read_text()), args.log)
    for line in sys.stdin:
        message = json.loads(line)
        if "id" not in message:
            continue
        try:
            response = {"result": server.handle(message)}
        except ValueError as error:
            response = {"error": {"code": -32601, "message": str(error)}}
        print(json.dumps({"jsonrpc": "2.0", "id": message["id"], **response}), flush=True)


if __name__ == "__main__":
    main()
