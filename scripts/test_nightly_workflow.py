#!/usr/bin/env python3
import re
from pathlib import Path


workflow = Path(".github/workflows/nightly.yml")
if not workflow.is_file():
    raise SystemExit(f"missing {workflow}")

text = workflow.read_text(encoding="utf-8")
required = [
    "branches: [master]",
    "workflow_dispatch:",
    "cancel-in-progress: false",
    "artifact: jcode-linux-x86_64",
    "artifact: jcode-linux-aarch64",
    "scripts/build_linux_compat.sh dist",
    "cargo +nightly build -Z build-std=std,panic_abort",
    "pattern: jcode-linux-*",
    "sha256sum jcode-linux-aarch64.tar.gz jcode-linux-x86_64.tar.gz > SHA256SUMS",
    'git tag -f nightly "$GITHUB_SHA"',
    "git push origin refs/tags/nightly --force",
    'gh release upload nightly \\',
    "dist/jcode-linux-x86_64.tar.gz",
    "dist/jcode-linux-aarch64.tar.gz",
    "dist/SHA256SUMS",
]

missing = [needle for needle in required if needle not in text]
if missing:
    raise SystemExit("nightly workflow missing:\n" + "\n".join(missing))


def job_block(name: str) -> str:
    marker = f"  {name}:\n"
    start = text.find(marker)
    if start < 0:
        raise SystemExit(f"nightly workflow missing job: {name}")
    next_job = re.search(r"^  [A-Za-z0-9_-]+:\s*$", text[start + len(marker) :], re.MULTILINE)
    if next_job is None:
        return text[start:]
    return text[start : start + len(marker) + next_job.start()]


publish = job_block("publish")
for needle in ["needs: build", "--prerelease", "--clobber"]:
    if needle not in publish:
        raise SystemExit(f"publish job missing: {needle}")

for forbidden in ["secrets.DEPLOY_KEY", "webfactory/ssh-agent"]:
    if forbidden in text:
        raise SystemExit(f"nightly workflow must not require: {forbidden}")

print("test_nightly_workflow: ok")
