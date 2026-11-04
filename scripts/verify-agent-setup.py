#!/usr/bin/env python3
"""Verify the repository's shared plugin installation without network access."""

import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys
from urllib.parse import unquote


ROOT = Path(__file__).resolve().parent.parent
SHARED = ROOT / ".agents"


def verify_local_references():
    forbidden = re.compile(
        r"\.cursor(?:/|-plugin)|\bCursor\b|https?://(?:[^/]+\.)?cursor\.(?:com|sh)/|"
        r"\bpstack:|\bgeneralPurpose\b|\b(?:subagent_type|readonly|environment|cloud_base_branch|"
        r"run_in_background|file_attachments):|\b(?:SendToUser|AskQuestion)\b|"
        r"pstack-models\.mdc|grok-\d|claude-opus-5-5-xhigh|/(?:add-plugin|in-cloud)\b"
    )
    documents = [SHARED / "README.md"]
    for component in ("skills", "agents", "docs", "rules"):
        documents.extend(p for p in (SHARED / component).rglob("*.md") if "scripts" not in p.relative_to(SHARED).parts)
    links = 0
    for document in documents:
        text = document.read_text()
        if document != SHARED / "README.md" and "rules" not in document.relative_to(SHARED).parts:
            assert not forbidden.search(text), f"Runtime-specific dependency: {document}"
        prose = re.sub(r"```.*?```", "", text, flags=re.S)
        for target in re.findall(r"\[[^\]]*\]\(([^)]+)\)", prose):
            if re.match(r"(?:[a-z][a-z0-9+.-]*:|#)", target):
                continue
            if any(char in target for char in "<>{}*"):
                continue
            relative = unquote(target.split("#", 1)[0])
            assert (document.parent / relative).exists(), f"Broken local link in {document}: {target}"
            links += 1
    return len(documents), links


def verify():
    subprocess.run([sys.executable, str(ROOT / "scripts/sync-agent-plugins.py"), "--check"], check=True)
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
                document = exposed / "SKILL.md" if component == "skills" else exposed
                assert document.is_file(), f"Missing definition: {document}"
                assert document.read_text().startswith("---\n"), f"Missing frontmatter: {document}"
                assert not document.is_symlink(), f"Active instructions must be local adaptations: {document}"
                counts[component] += 1
        actual = {
            str(path.relative_to(vendor)): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in vendor.rglob("*") if path.is_file()
        }
        assert actual == sources["files"][plugin], f"Upstream copy changed: {plugin}"
        for relative in sources["executables"][plugin]:
            assert (vendor / relative).stat().st_mode & 0o111, f"Missing executable mode: {relative}"
    local_skills = set(p.name for p in (SHARED / "skills").iterdir())
    bundled_skills = {p.name for plugin in sources["plugins"] for p in (SHARED / "plugins" / plugin / "skills").iterdir()}
    assert "create-skill" in local_skills, "Missing local skill authoring workflow"
    for skill in local_skills - bundled_skills:
        document = SHARED / "skills" / skill / "SKILL.md"
        assert document.is_file() and document.read_text().startswith("---\n"), f"Invalid local skill: {skill}"
    assert len(list((SHARED / "agents").iterdir())) == counts["agents"], "Unexpected agent entries"
    counts["skills"] = len(local_skills)

    config = (SHARED / "rules/pstack-models.md").read_text()
    presets = json.loads(re.search(r"```json\n(.*?)\n```", config, re.S).group(1))
    roles = dict(re.findall(r"^([a-z][a-z ,\-]+): (.+)$", config, re.M))
    setup = (SHARED / "plugins/pstack/skills/setup-pstack/SKILL.md").read_text()
    sample = setup.split("```", 2)[1].split("---", 2)[2]
    upstream_roles = {role for role, _ in re.findall(r"^([a-z][a-z ,\-]+): (.+)$", sample, re.M)}
    assert set(roles) == upstream_roles, "Model assignments must cover the current pstack roles"
    assert len(presets) == 5, "Expected five model presets"
    for role, choices in roles.items():
        names = choices.split(", ")
        assert all(name in presets for name in names), f"Unknown preset for {role}"
    for preset in presets.values():
        assert set(preset) == {"providerInstanceId", "model", "options"}, "Invalid T3 target"
    for rule in ("no-inline-imports", "typescript-exhaustive-switch", "plugin-compatibility", "pstack-models"):
        assert (claude / "rules" / f"{rule}.md").is_file(), f"Missing shared rule: {rule}"
    assert "`.agents/rules/*.md`" in (ROOT / "AGENTS.md").read_text(), "Codex must load shared rules"
    documents, links = verify_local_references()
    print(f"Verified {counts['skills']} skills, {counts['agents']} agents, {len(presets)} presets, {len(roles)} roles, and upstream file hashes.")
    print(f"Checked {documents} active documents and {links} local links for broken references and unsupported runtime assumptions.")


if __name__ == "__main__":
    verify()
