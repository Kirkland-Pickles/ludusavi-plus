#!/usr/bin/env python3
"""Download and stage the pinned Windows Rclone release."""

import argparse
import hashlib
from pathlib import Path
import shutil
import tempfile
from urllib.request import urlopen
import zipfile


RCLONE_VERSION = "1.75.2"
RCLONE_ARCHITECTURES = {
    "x86": ("386", "904876ca77f99f29400dfbb41ff99c132f64f1cdfa50fe7dc7923683b252186d"),
    "x64": ("amd64", "8a9db2bad1dc79a65b3348fd857069598e2aab7cf4d118b77b1df23055aff818"),
}


def download_rclone(architecture: str, destination: Path) -> Path:
    if architecture not in RCLONE_ARCHITECTURES:
        raise ValueError(f"Unsupported Windows architecture: {architecture}")
    upstream_architecture, expected_hash = RCLONE_ARCHITECTURES[architecture]
    archive_name = f"rclone-v{RCLONE_VERSION}-windows-{upstream_architecture}.zip"
    url = f"https://github.com/rclone/rclone/releases/download/v{RCLONE_VERSION}/{archive_name}"

    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="ludusavi-rclone-") as temporary:
        archive = Path(temporary) / archive_name
        hasher = hashlib.sha256()
        with urlopen(url, timeout=120) as response, archive.open("wb") as output:
            while chunk := response.read(1024 * 1024):
                output.write(chunk)
                hasher.update(chunk)

        actual_hash = hasher.hexdigest()
        if actual_hash != expected_hash:
            raise RuntimeError(f"Rclone SHA-256 mismatch for {archive_name}: {actual_hash}")

        member = f"rclone-v{RCLONE_VERSION}-windows-{upstream_architecture}/rclone.exe"
        with zipfile.ZipFile(archive) as bundle:
            try:
                info = bundle.getinfo(member)
            except KeyError as error:
                raise RuntimeError(f"Rclone archive is missing {member}") from error
            temporary_executable = destination.with_name(destination.name + ".part")
            try:
                with bundle.open(info) as source, temporary_executable.open("wb") as output:
                    shutil.copyfileobj(source, output)
                temporary_executable.replace(destination)
            finally:
                temporary_executable.unlink(missing_ok=True)

    return destination


def prepare_rclone_bundle(architecture: str, destination: Path, license_bundle: Path) -> Path:
    expected_version = license_bundle / "RCLONE-VERSION"
    if not expected_version.is_file() or expected_version.read_text(encoding="utf-8").strip() != RCLONE_VERSION:
        raise RuntimeError(f"Rclone license bundle must be generated from v{RCLONE_VERSION}")

    required = [
        "COPYING",
        "THIRD-PARTY-NOTICES.txt",
        "THIRD-PARTY-LICENSES.csv",
    ]
    for name in required:
        if not (license_bundle / name).is_file():
            raise RuntimeError(f"Rclone license bundle is missing {name}")
    licenses = license_bundle / "licenses"
    if not licenses.is_dir() or not any(path.is_file() for path in licenses.rglob("*")):
        raise RuntimeError("Rclone license bundle contains no license texts")
    if destination.exists():
        raise FileExistsError(destination)

    destination.mkdir(parents=True)
    download_rclone(architecture, destination / "rclone.exe")
    for name in required:
        shutil.copyfile(license_bundle / name, destination / name)
    shutil.copytree(licenses, destination / "licenses")
    return destination


def package_windows_portable(
    binary: Path,
    architecture: str,
    version: str,
    output: Path,
    license_bundle: Path,
) -> Path:
    if architecture not in RCLONE_ARCHITECTURES:
        raise ValueError(f"Unsupported Windows architecture: {architecture}")
    if not binary.is_file():
        raise FileNotFoundError(binary)

    artifact_architecture = {"x86": "win32", "x64": "win64"}[architecture]
    output.mkdir(parents=True, exist_ok=True)
    destination = output / f"ludusavi-v{version}-{artifact_architecture}.zip"
    if destination.exists():
        raise FileExistsError(destination)

    with tempfile.TemporaryDirectory(prefix="ludusavi-portable-") as temporary:
        staging = Path(temporary) / "bundle"
        staging.mkdir()
        shutil.copyfile(binary, staging / "ludusavi.exe")
        shutil.copyfile(Path(__file__).resolve().parents[1] / "LICENSE", staging / "LICENSE")
        prepare_rclone_bundle(architecture, staging / "Rclone", license_bundle)

        partial = destination.with_name(destination.name + ".part")
        try:
            with zipfile.ZipFile(partial, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
                for path in sorted(staging.rglob("*")):
                    if path.is_file():
                        archive.write(path, path.relative_to(staging).as_posix())
            partial.replace(destination)
        finally:
            partial.unlink(missing_ok=True)

    return destination


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    download = subparsers.add_parser("download", help="Download and verify the pinned Rclone executable")
    download.add_argument("architecture", choices=RCLONE_ARCHITECTURES)
    download.add_argument("destination", type=Path)
    portable = subparsers.add_parser("portable", help="Package a Windows portable release")
    portable.add_argument("binary", type=Path)
    portable.add_argument("--architecture", choices=RCLONE_ARCHITECTURES, required=True)
    portable.add_argument("--version", required=True)
    portable.add_argument("--output", type=Path, required=True)
    portable.add_argument("--rclone-licenses", type=Path, required=True)
    arguments = parser.parse_args()
    if arguments.command == "download":
        print(download_rclone(arguments.architecture, arguments.destination.resolve()))
    else:
        print(package_windows_portable(
            arguments.binary.resolve(strict=True), arguments.architecture, arguments.version,
            arguments.output.resolve(), arguments.rclone_licenses.resolve(strict=True),
        ))
