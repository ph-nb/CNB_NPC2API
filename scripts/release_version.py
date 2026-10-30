"""Calculate a deterministic release version from the base version and CI run."""

import os
import re
import tomllib
from pathlib import Path


def version_for_run(base, run_number):
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", base):
        raise ValueError("package version must be a stable three-part semantic version")
    if run_number < 1:
        raise ValueError("run number must be positive")
    major, minor, patch = (int(part) for part in base.split("."))
    return f"{major}.{minor}.{patch + run_number}"


def main():
    manifest = tomllib.loads(Path("Cargo.toml").read_text(encoding="utf-8"))
    print(version_for_run(manifest["package"]["version"], int(os.environ["GITHUB_RUN_NUMBER"])))


if __name__ == "__main__":
    main()
