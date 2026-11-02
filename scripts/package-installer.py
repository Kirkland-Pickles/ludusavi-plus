#!/usr/bin/env python3
"""Build native installers from a Ludusavi binary."""

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib

from rclone_bundle import prepare_rclone_bundle


ROOT = Path(__file__).resolve().parent.parent


def linux_root(binary: Path, root: Path, version: str) -> None:
    files = {
        binary: "usr/bin/ludusavi",
        ROOT / "LICENSE": "usr/share/doc/ludusavi/copyright",
        ROOT / "assets/linux/com.mtkennerly.ludusavi.desktop":
            "usr/share/applications/com.mtkennerly.ludusavi.desktop",
        ROOT / "assets/icon.png":
            "usr/share/icons/hicolor/64x64/apps/com.mtkennerly.ludusavi.png",
        ROOT / "assets/linux/com.mtkennerly.ludusavi.metainfo.xml":
            "usr/share/metainfo/com.mtkennerly.ludusavi.metainfo.xml",
    }
    for source, destination in files.items():
        target = root / destination
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, target)
        target.chmod(0o755 if destination == "usr/bin/ludusavi" else 0o644)
    control = root / "DEBIAN/control"
    control.parent.mkdir()
    control.write_text(
        f"Package: ludusavi\nVersion: {version}\nArchitecture: amd64\n"
        "Maintainer: Ludusavi fork maintainers\nSection: games\nPriority: optional\n"
        "Depends: libc6 (>= 2.35), libgtk-3-0, libxcb-composite0, libxkbcommon0, libxkbcommon-x11-0\n"
        "Description: Back up and restore PC game saves\n"
        " Can back up saves automatically after a game closes.\n",
        encoding="utf-8",
    )
    control.chmod(0o644)


def package(
    binary: Path,
    platform: str,
    architecture: str,
    output: Path,
    version: str,
    release: bool = False,
    rclone_licenses: Path | None = None,
) -> Path:
    if platform == "macos" and release:
        required = ["LUDUSAVI_MACOS_SIGNING_IDENTITY", "LUDUSAVI_MACOS_INSTALLER_IDENTITY", "LUDUSAVI_MACOS_NOTARY_PROFILE"]
        missing = [name for name in required if not os.environ.get(name)]
        if missing:
            raise RuntimeError("A macOS release requires: " + ", ".join(missing))
    output.mkdir(parents=True, exist_ok=True)
    stem = f"ludusavi-{version}-{architecture}"
    if platform == "windows":
        if rclone_licenses is None:
            raise RuntimeError("Windows packages require --rclone-licenses from the pinned Rclone source")
        compiler = os.environ.get("ISCC") or shutil.which("ISCC")
        if not compiler:
            raise RuntimeError("Install Inno Setup 6 and set ISCC to its ISCC.exe path")
        with tempfile.TemporaryDirectory(prefix="ludusavi-rclone-") as temporary:
            rclone_dir = Path(temporary) / "Rclone"
            prepare_rclone_bundle(architecture, rclone_dir, rclone_licenses)
            subprocess.run([
                compiler, f"/DBinary={binary}", f"/DRcloneDir={rclone_dir}",
                f"/DVersion={version}", f"/DArchitecture={architecture}",
                f"/DOutput={output}", str(ROOT / "installer/windows.iss"),
            ], check=True)
        result = output / f"{stem}-setup.exe"
    else:
        with tempfile.TemporaryDirectory(prefix="ludusavi-installer-") as temporary:
            staging = Path(temporary)
            if platform == "linux":
                linux_root(binary, staging, version)
                result = output / f"{stem}.deb"
                subprocess.run(["dpkg-deb", "--root-owner-group", "--build", str(staging), str(result)], check=True)
            else:
                app_staging = staging / "bundle"
                subprocess.run([
                    "bash", str(ROOT / "scripts/package-macos-app.sh"), str(binary), version,
                    str(app_staging), str(staging / "bundle.tar.gz"),
                ], check=True)
                payload = staging / "payload/Applications"
                payload.mkdir(parents=True)
                shutil.copytree(app_staging / "Ludusavi.app", payload / "Ludusavi.app")
                result = output / (f"{stem}.pkg" if release else f"{stem}-unsigned.pkg")
                arguments = [
                    "pkgbuild", "--root", str(staging / "payload"), "--install-location", "/",
                    "--identifier", "com.mtkennerly.ludusavi.fork", "--version", version,
                ]
                if release:
                    arguments += ["--sign", os.environ["LUDUSAVI_MACOS_INSTALLER_IDENTITY"]]
                subprocess.run(arguments + [str(result)], check=True)
                if release:
                    notarization = subprocess.run(["xcrun", "notarytool", "submit", str(result), "--keychain-profile",
                                                   os.environ["LUDUSAVI_MACOS_NOTARY_PROFILE"], "--wait", "--output-format", "json"],
                                                  check=True, capture_output=True, text=True)
                    status = json.loads(notarization.stdout)
                    if status.get("status") != "Accepted":
                        raise RuntimeError(f"macOS notarization was not accepted: {status}")
                    subprocess.run(["xcrun", "stapler", "staple", str(result)], check=True)
                    subprocess.run(["pkgutil", "--check-signature", str(result)], check=True)
                else:
                    print("Unsigned macOS package for development/testing; Gatekeeper distribution requires --release.")
    if not result.is_file():
        raise RuntimeError(f"Packaging did not produce {result}")
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("platform", choices=["windows", "linux", "macos"])
    parser.add_argument("binary", type=Path)
    parser.add_argument("--architecture", required=True, choices=["x86", "x64", "arm64"])
    parser.add_argument("--output", type=Path, default=ROOT / "dist")
    parser.add_argument("--release", action="store_true", help="Require Developer ID signing and notarization for macOS")
    parser.add_argument("--rclone-licenses", type=Path, help="Output of collect-rclone-licenses.py for Windows packages")
    arguments = parser.parse_args()
    binary = arguments.binary.resolve(strict=True)
    expected = {"windows": {"x86", "x64"}, "linux": {"x64"}, "macos": {"arm64", "x64"}}
    if arguments.architecture not in expected[arguments.platform]:
        parser.error("Unsupported platform/architecture combination")
    version = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["package"]["version"]
    licenses = arguments.rclone_licenses.resolve(strict=True) if arguments.rclone_licenses else None
    print(package(
        binary, arguments.platform, arguments.architecture, arguments.output.resolve(),
        version, arguments.release, licenses,
    ))
