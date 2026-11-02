#!/usr/bin/env python3
"""Collect license texts and notices for the pinned Rclone Windows build."""

import argparse
import os
from pathlib import Path
import shutil
import subprocess

from rclone_bundle import RCLONE_VERSION

GO_LICENSES_VERSION = "v2.0.1"
RCLONE_REVISION = "be8d6044eacb843b74d335db0c5fbe09222031fe"
GO_ARCHITECTURES = {"x86": "386", "x64": "amd64"}


def collect(source: Path, architecture: str, output: Path) -> Path:
    if architecture not in GO_ARCHITECTURES:
        raise ValueError(f"Unsupported Windows architecture: {architecture}")
    if not (source / "COPYING").is_file() or not (source / "go.mod").is_file():
        raise RuntimeError(f"Not an Rclone source checkout: {source}")
    if "module github.com/rclone/rclone" not in (source / "go.mod").read_text(encoding="utf-8"):
        raise RuntimeError(f"Unexpected Go module in {source / 'go.mod'}")
    revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=source, text=True).strip()
    if revision != RCLONE_REVISION:
        raise RuntimeError(f"Expected Rclone v{RCLONE_VERSION} source at {RCLONE_REVISION}, got {revision}")
    go = shutil.which("go")
    if not go:
        raise RuntimeError("Go is required to collect Rclone dependency licenses")
    if output.exists():
        raise FileExistsError(output)

    output.mkdir(parents=True)
    licenses = output / "licenses"
    host_env = os.environ.copy()
    subprocess.run(
        [go, "install", f"github.com/google/go-licenses/v2@{GO_LICENSES_VERSION}"],
        cwd=source,
        env=host_env,
        check=True,
    )
    gobin = subprocess.check_output([go, "env", "GOBIN"], env=host_env, text=True).strip()
    if not gobin:
        gobin = str(Path(subprocess.check_output([go, "env", "GOPATH"], env=host_env, text=True).strip()) / "bin")
    go_licenses = Path(gobin) / "go-licenses.exe"
    if not go_licenses.is_file():
        raise RuntimeError(f"go-licenses was not installed at {go_licenses}")

    target_env = host_env.copy()
    target_env["GOOS"] = "windows"
    target_env["GOARCH"] = GO_ARCHITECTURES[architecture]
    target_env["CGO_ENABLED"] = "0"
    target_env["GOFLAGS"] = "-tags=cmount"
    if architecture == "x86":
        target_env["GO386"] = "softfloat"
    subprocess.run(
        [str(go_licenses), "save", ".", f"--save_path={licenses}"],
        cwd=source,
        env=target_env,
        check=True,
    )
    report = subprocess.run(
        [str(go_licenses), "report", "."],
        cwd=source,
        env=target_env,
        check=True,
        capture_output=True,
        text=True,
    )
    (output / "THIRD-PARTY-LICENSES.csv").write_text(
        "Name,LicenseURL,LicenseName\n" + report.stdout,
        encoding="utf-8",
        newline="\n",
    )
    shutil.copyfile(source / "COPYING", output / "COPYING")
    (output / "RCLONE-VERSION").write_text(RCLONE_VERSION + "\n", encoding="utf-8")

    goroot = Path(subprocess.check_output([go, "env", "GOROOT"], env=host_env, text=True).strip())
    go_license = licenses / "Go"
    go_license.mkdir()
    shutil.copyfile(goroot / "LICENSE", go_license / "LICENSE")
    (output / "THIRD-PARTY-NOTICES.txt").write_text(
        f"""Rclone v{RCLONE_VERSION} is distributed as an unmodified official Windows binary.
Rclone's own MIT license is included in COPYING.

Rclone includes third-party Go dependencies. Their license texts and notices are
included under licenses/ and the dependency/license inventory is in
THIRD-PARTY-LICENSES.csv. The inventory was collected from the upstream Rclone
v{RCLONE_VERSION} source for Windows/{GO_ARCHITECTURES[architecture]} using
google/go-licenses {GO_LICENSES_VERSION}. The Go standard-library license is
included at licenses/Go/LICENSE.
""",
        encoding="utf-8",
        newline="\n",
    )
    return output


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--architecture", choices=GO_ARCHITECTURES, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    print(collect(args.source.resolve(strict=True), args.architecture, args.output.resolve()))
