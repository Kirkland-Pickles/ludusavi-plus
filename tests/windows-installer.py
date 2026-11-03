#!/usr/bin/env python3
"""Test installation under temporary paths and unique registration keys."""

import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import uuid
import winreg
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "scripts"))
from rclone_bundle import RCLONE_VERSION, package_windows_portable


def run(binary, compiler, rclone_licenses, user_data=False):
    repo = Path(__file__).resolve().parent.parent
    if user_data and os.environ.get("CI") != "true":
        raise RuntimeError("The normal AppData test requires a disposable CI account")
    rclone_licenses = rclone_licenses.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="ludusavi-install-test-") as temporary:
        root = Path(temporary)
        identity = str(uuid.uuid4())
        registry = rf"Software\Microsoft\Windows\CurrentVersion\App Paths\ludusavi-test-{identity}.exe"
        portable = package_windows_portable(binary, "x64", "0.31.0", root / "portable", rclone_licenses)
        with zipfile.ZipFile(portable) as archive:
            members = set(archive.namelist())
            assert {
                "ludusavi.exe",
                "LICENSE",
                "Rclone/rclone.exe",
                "Rclone/COPYING",
                "Rclone/THIRD-PARTY-NOTICES.txt",
                "Rclone/THIRD-PARTY-LICENSES.csv",
                "Rclone/licenses/Go/LICENSE",
            }.issubset(members), "Portable archive is missing its executable or license payload"
            archive.extractall(root / "portable-content")
        rclone_dir = root / "portable-content" / "Rclone"
        rclone_executable = rclone_dir / "rclone.exe"
        rclone_version = subprocess.check_output(
            [str(rclone_executable), "--version"], text=True,
            env={**os.environ, "RCLONE_CONFIG": str(root / "rclone.conf")},
        )
        assert rclone_version.startswith(f"rclone v{RCLONE_VERSION}"), rclone_version
        notices = (rclone_dir / "THIRD-PARTY-NOTICES.txt").read_text(encoding="utf-8")
        assert f"Rclone v{RCLONE_VERSION}" in notices and "third-party Go dependencies" in notices

        script = (repo / "installer/windows.iss").read_text(encoding="utf-8")
        script = script.replace("78A48778-C44A-45EB-AED3-62D625E21970", identity)
        script = script.replace(r"App Paths\ludusavi.exe", rf"App Paths\ludusavi-test-{identity}.exe")
        script = script.replace('ValueName: "Ludusavi"', f'ValueName: "Ludusavi Test {identity}"')
        script = script.replace(r"..\LICENSE", str(repo / "LICENSE"))
        script = script.replace(r"..\assets\icon.ico", str(repo / "assets/icon.ico"))
        test_script = root / "test.iss"
        test_script.write_text(script, encoding="utf-8")
        for version in ["0.31.0", "0.31.1"]:
            subprocess.run([
                str(compiler), f"/DBinary={binary}", f"/DRcloneDir={rclone_dir}",
                f"/DVersion={version}", "/DArchitecture=x64",
                f"/DOutput={root}", str(test_script),
            ], check=True, stdout=subprocess.DEVNULL)
        install = root / "installed"
        configuration = root / "configuration"
        if user_data:
            appdata = ctypes.create_unicode_buffer(32768)
            assert ctypes.windll.shell32.SHGetFolderPathW(None, 26, None, 0, appdata) == 0
            configuration = Path(appdata.value) / "ludusavi"
            assert not configuration.exists(), "The test account already has Ludusavi data"
        created_configuration = False
        try:
            configuration.mkdir()
            created_configuration = True
            isolated = [str(binary), "--config", str(root / "defaults"), "--no-manifest-update"]
            config = json.loads(subprocess.check_output(isolated + ["config", "show", "--api"], text=True))
            config["backup"]["path"] = str(root / "backups")
            config["restore"]["path"] = str(root / "restore")
            config["release"]["check"] = False
            config["theme"] = "dark"
            config["customGames"] = [{"name": "Installer Test", "files": [str(root / "save.txt")]}]
            config["roots"] = [{"store": "other", "path": str(root / "games")}]
            config["apps"]["rclone"]["path"] = str(root / "custom-rclone.exe")
            config["apps"]["rclone"]["arguments"] = "--fast-list"
            configured = config
            sentinel = configuration / "config.yaml"
            sentinel.write_text(json.dumps(config), encoding="utf-8")
            config = json.loads(subprocess.check_output([
                str(binary), "--config", str(configuration), "--no-manifest-update", "config", "show", "--api",
            ], text=True))
            assert config["backup"]["path"] == configured["backup"]["path"]
            assert config["restore"]["path"] == configured["restore"]["path"]
            assert config["release"]["check"] is False
            assert config["theme"] == "dark"
            assert config["customGames"][0]["name"] == "Installer Test"
            assert config["customGames"][0]["files"] == configured["customGames"][0]["files"]
            assert config["roots"] == configured["roots"]
            assert config["apps"]["rclone"]["path"] == configured["apps"]["rclone"]["path"]
            assert config["apps"]["rclone"]["arguments"] == "--fast-list"
            expected = sentinel.read_bytes()
            for version in ["0.31.0", "0.31.1"]:
                subprocess.run([
                    str(root / f"ludusavi-{version}-x64-setup.exe"), "/CURRENTUSER", "/VERYSILENT",
                    "/SUPPRESSMSGBOXES", "/NORESTART", f"/DIR={install}", f"/GROUP=Ludusavi Test {identity}",
                ], check=True)
                assert hashlib.sha256(binary.read_bytes()).digest() == hashlib.sha256((install / "ludusavi.exe").read_bytes()).digest()
                assert hashlib.sha256(rclone_executable.read_bytes()).digest() == hashlib.sha256((install / "Rclone/rclone.exe").read_bytes()).digest()
                assert (install / "Rclone/COPYING").read_bytes() == (rclone_dir / "COPYING").read_bytes()
                assert sentinel.read_bytes() == expected, "Installation or upgrade changed configuration"
                command = [str(install / "ludusavi.exe"), "--no-manifest-update"]
                if not user_data:
                    command += ["--config", str(configuration)]
                current = json.loads(subprocess.check_output(command + ["config", "show", "--api"], text=True))
                assert current == config, "Installed app used the wrong configuration or changed saved settings"
                assert sentinel.read_bytes() == expected, "Reading migrated configuration changed its contents"
                fresh = json.loads(subprocess.check_output([
                    str(install / "ludusavi.exe"), "--config", str(root / "fresh-config"),
                    "--no-manifest-update", "config", "show", "--api",
                ], text=True))
                assert Path(fresh["apps"]["rclone"]["path"]) == install / "Rclone/rclone.exe", \
                    "Installed app did not discover bundled Rclone"
            with winreg.OpenKey(winreg.HKEY_CURRENT_USER, registry) as key:
                assert winreg.QueryValue(key, None) == str(install / "ludusavi.exe")
            subprocess.run([str(install / "ludusavi.exe"), "--version"], check=True)
            subprocess.run([str(install / "unins000.exe"), "/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"], check=True)
            assert not (install / "ludusavi.exe").exists()
            assert not (install / "Rclone/rclone.exe").exists()
            assert not (install / "Rclone/COPYING").exists()
            assert sentinel.read_bytes() == expected, "Uninstall changed configuration"
            try:
                with winreg.OpenKey(winreg.HKEY_CURRENT_USER, registry):
                    raise AssertionError("Uninstaller left its App Paths key")
            except FileNotFoundError:
                pass
            print("Install, upgrade, configuration loading, uninstall, and configuration preservation passed")
        finally:
            if (install / "ludusavi.exe").exists() and (install / "unins000.exe").exists():
                subprocess.run([str(install / "unins000.exe"), "/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"], check=True)
            # Inno's helper removes the running uninstaller after it exits.
            for _ in range(25):
                if not (install / "unins000.exe").exists():
                    break
                time.sleep(0.2)
            if user_data and created_configuration and configuration.exists():
                shutil.rmtree(configuration)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("compiler", type=Path)
    parser.add_argument("--rclone-licenses", type=Path, required=True)
    parser.add_argument("--user-data", action="store_true", help="Check the normal AppData folder on a disposable CI account")
    args = parser.parse_args()
    run(args.binary.resolve(strict=True), args.compiler.resolve(strict=True),
        args.rclone_licenses.resolve(strict=True), args.user_data)
