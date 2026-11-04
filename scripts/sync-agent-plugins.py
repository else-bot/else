#!/usr/bin/env python3
"""Generate portable local instructions from the pinned plugin snapshots."""

import argparse
import os
from pathlib import Path
import re


ROOT = Path(__file__).resolve().parent.parent
SHARED = ROOT / ".agents"
ADAPTATIONS = SHARED / "adaptations"


def replace_paragraph(text, prefix, replacement):
    pattern = r"^" + re.escape(prefix) + r"[^\n]*(?:\n(?!\n)[^\n]+)*"
    result, count = re.subn(pattern, lambda _: replacement, text, count=1, flags=re.M)
    assert count == 1, f"Upstream paragraph changed: {prefix}"
    return result


def adapt(relative, text):
    path = relative.as_posix()
    if path == "agents/poteto-agent.md":
        text = replace_paragraph(text, "You are operating as poteto-mode's full agent style.",
            "Read `.agents/skills/poteto-mode/SKILL.md` in full before starting, including "
            "its Principles index. Use the absolute path your parent supplies, or resolve "
            "that repository-local path. Read each applicable principle's local SKILL.md. "
            "You are a delegate; the parent owns the playbook and delegation. "
            "Do not start child agents unless the brief explicitly authorizes them.")
    if path == "skills/poteto-mode/SKILL.md":
        text = replace_paragraph(text, "**Defaults for every `Task` call.**",
            "**Model roles.** Read `.agents/rules/pstack-models.md`. Resolve each role's "
            "preset into its provider, model, and options. Opus medium handles coding, "
            "judgment, and synthesis. Opus high handles the hardest tasks. "
            "Sol and Fable provide independent review and design perspectives.")
        text = replace_paragraph(text, "**Match the model to the job.**",
            "**Keep the selected preset.** Preserve its model, effort, and speed. "
            "Do not construct model slugs, raise or lower effort, or substitute a model. "
            "Report an unavailable preset and continue only with independent work. "
            "Follow the current runtime's delegation and completion instructions.")
    if path in {"skills/how/SKILL.md", "skills/why/SKILL.md", "skills/reflect/SKILL.md"}:
        prefix = {"how": "Every spawn is a Task subagent", "why": "Each spawn below names a role line", "reflect": "Each reviewer and the synthesizer name a role line"}[relative.parts[1]]
        text = replace_paragraph(text, prefix,
            "Read `.agents/rules/pstack-models.md` and resolve the role named below "
            "through its preset table. Preserve the configured provider, effort, and speed. "
            "Do not pass preset names as model IDs or silently fall back to another model. "
            "Keep read-only investigators able to use the required tools and MCPs; "
            "their briefs must prohibit edits.")
        if path == "skills/how/SKILL.md":
            text = text.replace("## 1. Assess complexity", "Explorers use `how explorer`. Explainers use `how explainer`.\n\n## 1. Assess complexity")
        if path == "skills/why/SKILL.md":
            text = replace_paragraph(text, "Every spawn is `generalPurpose`",
                "Use agents with the required tools and MCP access. Investigator briefs prohibit edits.")
            text = replace_paragraph(text, "Before spawning investigators, list the available MCPs",
                "Discover the current session's tools and MCPs through its tool catalog or "
                "tool search. Do not assume a provider-specific MCP directory exists.")
        if path == "skills/reflect/SKILL.md":
            text = replace_paragraph(text, "One message, three `Task` calls,",
                "Spawn three reviewers using the current runtime's delegation tools. "
                "Allow the MCP access each investigation requires; prohibit file edits in each brief.")
            text = replace_paragraph(text, "One `Task` call, `subagent_type:",
                "Spawn one synthesizer using `reflect judgment, divergent, synthesizer`. "
                "It spot-verifies citations through the available MCPs. Pass "
                "`references/synthesizer.md` with all reviewer outputs. It returns an "
                "Accepted / Rejected / Backlog list.")
    if path == "skills/arena/SKILL.md":
        text = replace_paragraph(text, "3. Pick the runners.",
            "3. Read `arena runners` in `.agents/rules/pstack-models.md`. Spawn one "
            "candidate per configured entry, resolving each preset through its target table. "
            "Keep its exact model, effort, and speed. Report any unavailable seat.")
        text = replace_paragraph(text, "After all Phase B candidates complete,",
            "After all Phase B candidates complete, read `arena cross-judge pool` in "
            "`.agents/rules/pstack-models.md`. Select one available preset from a model "
            "family different from the parent, in pool order. Resolve the actual model "
            "ID before comparing families: `gpt-*` and `claude-*` are different families. "
            "Use the parent session's actual model ID, not its preset name. "
            "The judge must not edit files. It sees the rubric and candidates, scores each "
            "criterion, and recommends a base. If no cross-family preset is available, "
            "report that gap and select the base directly. Never substitute an unconfigured model.")
    if path == "skills/swarm/SKILL.md":
        text = replace_paragraph(text, "3. Pick the worker model",
            "3. Read `swarm workers` in `.agents/rules/pstack-models.md` and resolve "
            "its preset. Preserve its exact model, effort, and speed. For a model race, "
            "name each arm's configured preset before spawning.")
        text = replace_paragraph(text, "Spawn all N workers in one message",
            "Spawn workers using the current runtime's delegation tools and the step 3 "
            "preset. Give writers separate worktrees through runtime-supported isolation. "
            "Keep work on this machine when it needs local tools, credentials, or a running app.")
        text = text.replace("When a worker must start from a non-default pushed branch, pass `cloud_base_branch`.",
            "Name the starting branch and commit in each brief. Use the runtime's checkout options when supported.")
    if path == "skills/recall/SKILL.md":
        text = replace_paragraph(text, "Transcripts live at",
            "Use the current runtime's workspace-scoped transcript or thread tools. "
            "In T3, read a named thread before describing it and use its returned link. "
            "Do not guess transcript directories or search other workspaces. "
            "If history is unavailable, use this conversation and live repository state, and report the gap.")
    if path == "skills/architect/SKILL.md":
        text = replace_paragraph(text, "Take the runners from",
            "Read `architect runners` in `.agents/rules/pstack-models.md` instead of "
            "`arena runners`. Resolve every entry through its preset table. Follow "
            "Arena's candidate isolation rules and report any unavailable configured model.")
    if path == "skills/reflect/SKILL.md":
        start = text.index("Before fanning out, find this conversation's transcript")
        end = text.index("## 2. Spawn three reviewers", start)
        text = text[:start] + (
            "Locate this conversation through the current runtime's workspace-scoped "
            "history tools. In T3, read the current thread with its thread tools. "
            "Use an explicitly supplied transcript path when one exists. If history "
            "is unavailable, pass a digest of this conversation. Do not guess a "
            "transcript layout or search another workspace.\n\n") + text[end:]
        text = replace_paragraph(text, "Never edit a skill under `plugins/cache/`.",
            "Never edit the pinned snapshots in `.agents/plugins/`. For an approved "
            "bundled-skill change, edit the corresponding `.agents/adaptations/skills/` "
            "file and regenerate with `python scripts/sync-agent-plugins.py --write`. "
            "New project skills live directly in `.agents/skills/`. Never open a PR or push.")
        text = text.replace("Default `model`", "Example preset")
    if path == "docs/guide/02-poteto-mode.md":
        text = replace_paragraph(text, "If you run several agents against one repository",
            "Give concurrent writers separate worktrees, ports, and build directories. "
            "Use the current runtime's isolation options. A worktree separates files; "
            "it does not create a virtual machine or a separate desktop session.")
        text = text.replace("./01-setup.md#run-your-first-task", "./01-setup.md")
    if path == "docs/guide/09-make-it-yours.md":
        text = replace_paragraph(text, "One situational skill is for Grok Bot users.",
            "The local [`/make-bot-ui`](../../skills/make-bot-ui/SKILL.md) workflow "
            "builds a page for a bot webhook exposed by an available integration. "
            "Its server holds credentials outside the browser and chat. The workflow "
            "does not assume a hosted routine API or install network services.")
    if path == "docs/guide/07-overnight.md":
        text = replace_paragraph(text, "A [Cursor Project]",
            "A coordinator can manage a long task when the runtime provides persistent "
            "state and continuation events. Use a local decision trail and separate "
            "worktrees for writers. Do not assume that work continues after the machine "
            "or runtime stops. Start each task with `/poteto-mode` and a completion check.")
        text = text.replace("`/loop` is Cursor's built-in wake mechanism, not a pstack skill.",
            "Continuation depends on the runtime's supported event or scheduling tools.")
        text = text.replace("restart Cursor", "restart the agent runtime")
        text = replace_paragraph(text, "pstack ships this as a dormant [automation pack]",
            "Hosted automation packs are not activated by this local installation. "
            "Use only an integration exposed by the current runtime, and configure "
            "recurring or external actions only when the user requests them.")
        text = text.replace("- Drag related chats into the Project, finished ones included. They become context for every agent in it.",
            "- Put the relevant prior reports and artifact paths in each agent's brief.")
    if path == "skills/poteto-mode/playbooks/autonomous-run.md":
        text = replace_paragraph(text, "2. Pick the wake mechanism",
            "2. Use the runtime's supported event subscription or scheduling tools. "
            "In T3, PR waits use its PR watcher and end the turn. Schedule recurring "
            "work only when the user asks for it. If no continuation mechanism exists, "
            "report the limitation and preserve a checkpoint; do not claim a background run is armed.")
    if path == "skills/poteto-mode/playbooks/worktree-cleanup.md":
        text = text.replace("`.cursor/worktrees/myrepo/x`", "`.t3/worktrees/myrepo/x`")
        text = re.sub(r", `~/Library/Application Support/Cursor`.*?package caches", ", package caches", text)
        text += "\nSet `PSTACK_TRANSCRIPTS_DIR` to an explicitly workspace-scoped transcript directory if available. Without it, the audit reports no chat evidence and does not search a home directory.\n"

    replacements = [
        ("~/.cursor/rules/pstack-models.mdc", ".agents/rules/pstack-models.md"),
        ("`pstack-models.mdc`", "`.agents/rules/pstack-models.md`"),
        ("~/.cursor/projects/*/", "other workspaces' transcript directories"),
        ("~/.cursor/skills/", ".agents/skills/"),
        (".cursor/skills/", ".agents/skills/"),
        ("~/.cursor/plugins/", ".agents/"),
        ("Cursor's built-in `create-skill`", "the local `create-skill`"),
        ("Cursor's built-in for authoring SKILL.md files", "local instructions for authoring SKILL.md files"),
        (", not Cursor's built-in babysit skill, whose description matches the same words", ""),
        ("This playbook replaces Cursor's built-in babysit skill for these requests, so do not route there even though its description matches the same words. ", ""),
        ("Cursor cloud agent", "isolated delegated agent"),
        ("a new cloud VM (`environment: \"cloud\"`)", "a separate worktree through runtime-supported isolation"),
        ("its own cloud VM", "its own isolated worktree"),
        ("recent Cursor chats", "recent agent conversations"),
        ("a Cursor restart", "an agent runtime restart"),
        ("Cursor's `/loop` command", "the runtime's supported continuation mechanism"),
        ("the Project tracker (`notes.md`) and the Project store", "a repository-local decision trail and `notes.md`"),
        ("a isolated delegated agent", "an isolated delegated agent"),
        ("`/loop 1h`", "an explicitly requested hourly runtime schedule"),
        ("`/loop`", "the runtime's supported continuation mechanism"),
        ("/loop until", "Continue until"),
        ("/loop above", "the completion check above"),
        ("a Custom Mode keeps `/poteto-mode` in context on every turn", "explicit skill invocation loads the workflow for the current task"),
        ("cloud agents", "agents in separate worktrees"),
        ("cloud agent", "isolated delegated agent"),
        ("from `cursor-team-kit`", "from the local shared skills"),
        ("`cursor-team-kit` publishes", "The local shared installation provides"),
        ("the `cursor-team-kit` plugin", "the local team workflow bundle"),
        ("`AskQuestion`", "the current runtime's question tool"),
        ("AskQuestion", "the current runtime's question tool"),
        ("`Task`", "the current runtime's delegation tool"),
        ("Task subagent", "delegated agent"),
        ("via Task", "through delegation"),
        ("set their own `subagent_type`", "choose their own agent definitions"),
        ("`subagent_type: generalPurpose`", "a general-purpose agent"),
        ("`generalPurpose`", "a general-purpose agent"),
        ("grok-4.7-xhigh-fast", "sol-xhigh-fast"),
        ("claude-opus-5-5-xhigh", "opus-medium"),
        ("from the `cursor-team-kit` plugin", "from the local shared skills"),
        ("in the cursor-team-kit plugin", "in the local shared skills"),
        ("shipped in the cursor-team-kit plugin", "installed locally"),
        ("Cursor's built-in `create-skill` skill", "the local `create-skill` skill"),
        ("readonly strips MCPs", "each agent must retain the required MCP access"),
        ("readonly strips MCP", "each agent must retain the required MCP access"),
        ("Pick each subagent's reasoning level and fast or standard speed for the job, per the Subagents section of the **poteto-mode** skill.", "Preserve the role's configured preset and use the local runtime rules for delegation."),
        ("Pick each subagent's reasoning level and fast or standard speed for the job, per the Subagents section.", "Preserve the role's configured preset and use the local runtime rules for delegation."),
        ("Pick each subagent's reasoning level and fast or standard speed for the job, per the Subagents section", "Preserve the role's configured preset per the local runtime rules"),
    ]
    for old, new in replacements:
        text = text.replace(old, new)
    text = re.sub(r"^is_background:.*\n", "", text, flags=re.M)
    text = re.sub(r"^(?:icon|color):[^\n]*\n", "", text, flags=re.M)
    text = re.sub(r" \(default `(?:sol-xhigh-fast|opus-medium)`\)", "", text)
    text = re.sub(r", default `(?:sol-xhigh-fast|opus-medium)`", "", text)
    text = text.replace("model: fast\n", "model: inherit\n")
    text = re.sub(r"`readonly: (?:true|false)`", "a brief that prohibits edits", text)
    text = re.sub(r"`run_in_background: true`", "runtime-supported completion notifications", text)
    text = text.replace('`subagent_type: "poteto-agent"`', 'the local `poteto-agent` definition')
    text = text.replace('`subagent_type: "comment-sicko"`', 'the local `comment-sicko` definition')
    text = text.replace('`subagent_type: "shell"`', 'a shell-capable agent')
    text = text.replace('`subagent_type: "explore"`', 'a read-only explorer')
    text = text.replace('`subagent_type: "thermo-nuclear-code-quality-review"`', 'the local `thermo-nuclear-code-quality-review` definition')
    text = text.replace("in the the current runtime's delegation tool response body", "in the delegation result")
    if path == "skills/why/references/synthesizer-prompt.md":
        text = text.replace("[PR #123](url)", "[PR #123](<source-url>)")
    if path.startswith("skills/"):
        skill = SHARED / "skills" / relative.parts[1]
        def local_reference(match):
            command = match.group(1).split(" ", 1)
            target = skill / command[0]
            source = SHARED / "plugins/pstack/skills" / relative.parts[1] / command[0]
            if not source.exists():
                return match.group(0)
            command[0] = os.path.relpath(target, SHARED / relative.parent)
            return "`" + " ".join(command) + "`"
        text = re.sub(r"`((?:scripts|playbooks|references)/[^`]+)`", local_reference, text)
    if (path.startswith("skills/") and relative.name == "SKILL.md") or path.startswith("agents/"):
        end = text.index("\n---", 4) + 4
        runtime = os.path.relpath(SHARED / "rules/plugin-compatibility.md", SHARED / relative.parent)
        models = os.path.relpath(SHARED / "rules/pstack-models.md", SHARED / relative.parent)
        text = text[:end] + f"\n\nRead the [local runtime rules]({runtime}) and [model presets]({models}) before following this workflow.\n\n" + text[end:].lstrip("\n")
    return text.rstrip() + "\n"


def artifacts():
    for plugin in ("pstack", "cursor-team-kit"):
        vendor = SHARED / "plugins" / plugin
        groups = ("skills", "agents", "docs") if plugin == "pstack" else ("skills", "agents")
        for group in groups:
            yield from walk(vendor / group, Path(group))


def walk(source, relative):
    override = ADAPTATIONS / relative
    if source.is_dir():
        portable = any(p.suffix == ".md" and "scripts" not in p.relative_to(source).parts for p in source.rglob("*"))
        if not portable and not override.is_dir():
            yield relative, source, None
            return
        for child in sorted(source.iterdir()):
            yield from walk(child, relative / child.name)
    elif override.is_file():
        text = override.read_text()
        if source.suffix == ".md":
            text = adapt(relative, text)
        yield relative, source, text
    elif source.suffix == ".md" and "scripts" not in relative.parts:
        yield relative, source, adapt(relative, source.read_text())
    else:
        yield relative, source, None


def sync(check):
    stale = []
    for relative, source, content in artifacts():
        destination = SHARED / relative
        target = Path(os.path.relpath(source, destination.parent))
        if content is None:
            matches = destination.is_symlink() and destination.readlink() == target
        else:
            matches = destination.is_file() and not destination.is_symlink() and destination.read_text() == content
        if matches:
            continue
        stale.append(str(relative))
        if check:
            continue
        for parent in reversed(destination.parents):
            if parent == SHARED or SHARED in parent.parents:
                if parent.is_symlink():
                    parent.unlink()
                parent.mkdir(exist_ok=True)
        if destination.is_symlink():
            destination.unlink()
        if content is None:
            destination.symlink_to(target, target_is_directory=source.is_dir())
        else:
            destination.write_text(content)
            destination.chmod(source.stat().st_mode & 0o777)
    if check and stale:
        raise SystemExit("Stale local adaptations; run python scripts/sync-agent-plugins.py --write:\n" + "\n".join(stale))
    print(f"{'Checked' if check else 'Updated'} local plugin adaptations; {len(stale)} files {'differ' if check else 'written'}.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--check", action="store_true")
    action.add_argument("--write", action="store_true")
    sync(parser.parse_args().check)
