#!/usr/bin/env python3
"""Verify the repository's shared plugin installation without network access."""

import hashlib
import json
from pathlib import Path
import re


ROOT = Path(__file__).resolve().parent.parent
SHARED = ROOT / ".agents"


def verify():
    claude = ROOT / ".claude"
    assert claude.is_symlink() and claude.readlink() == Path(".agents"), ".claude must point to .agents"
    assert claude.resolve() == SHARED.resolve(), ".claude must resolve to the shared installation"
    sources = json.loads((SHARED / "sources.json").read_text())
    assert re.fullmatch(r"[0-9a-f]{40}", sources["revision"]), "Missing source commit"
    counts = {"skills": 0, "agents": 0}
    for plugin in sources["plugins"]:
        vendor = SHARED / "plugins" / plugin
        assert (vendor / "LICENSE").is_file(), f"Missing {plugin} license"
        for component in counts:
            for entry in (vendor / component).iterdir():
                exposed = SHARED / component / entry.name
                if component == "agents" and entry.name == "ci-watcher.md":
                    expected = entry.read_text().replace("model: fast\n", "model: inherit\n").replace("is_background: true\n", "")
                    assert exposed.read_text() == expected, "CI watcher must use compatible agent frontmatter"
                else:
                    assert exposed.is_symlink(), f"Missing discovery link: {exposed}"
                    assert not exposed.readlink().is_absolute(), f"Absolute link: {exposed}"
                    assert exposed.resolve() == entry.resolve(), f"Wrong discovery target: {exposed}"
                document = exposed / "SKILL.md" if component == "skills" else exposed
                assert document.is_file(), f"Missing definition: {document}"
                assert document.read_text().startswith("---\n"), f"Missing frontmatter: {document}"
                counts[component] += 1
        actual = {
            str(path.relative_to(vendor)): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in vendor.rglob("*") if path.is_file()
        }
        assert actual == sources["files"][plugin], f"Upstream copy changed: {plugin}"
        for relative in sources["executables"][plugin]:
            assert (vendor / relative).stat().st_mode & 0o111, f"Missing executable mode: {relative}"
    for component, count in counts.items():
        assert len(list((SHARED / component).iterdir())) == count, f"Unexpected {component} entries"

    config = (SHARED / "rules/pstack-models.md").read_text()
    presets = json.loads(re.search(r"```json\n(.*?)\n```", config, re.S).group(1))
    roles = dict(re.findall(r"^([a-z][a-z ,\-]+): (.+)$", config, re.M))
    setup = (SHARED / "skills/setup-pstack/SKILL.md").read_text()
    sample = setup.split("```", 2)[1].split("---", 2)[2]
    upstream_roles = {role for role, _ in re.findall(r"^([a-z][a-z ,\-]+): (.+)$", sample, re.M)}
    assert set(roles) == upstream_roles, "Model assignments must cover the current pstack roles"
    assert len(presets) == 5, "Expected five model presets"
    for role, choices in roles.items():
        names = choices.split(", ")
        assert all(name in presets for name in names), f"Unknown preset for {role}"
        if role in {"arena runners", "arena cross-judge pool", "architect runners", "interrogate reviewers"}:
            assert len(names) == 5 and set(names) == set(presets), f"Incomplete panel: {role}"
    for preset in presets.values():
        assert set(preset) == {"providerInstanceId", "model", "options"}, "Invalid T3 target"
    for rule in ("no-inline-imports", "typescript-exhaustive-switch", "plugin-compatibility", "pstack-models"):
        assert (claude / "rules" / f"{rule}.md").is_file(), f"Missing shared rule: {rule}"
    assert "`.agents/rules/*.md`" in (ROOT / "AGENTS.md").read_text(), "Codex must load shared rules"
    print(f"Verified {counts['skills']} skills, {counts['agents']} agents, {len(presets)} presets, {len(roles)} roles, and upstream file hashes.")


if __name__ == "__main__":
    verify()
