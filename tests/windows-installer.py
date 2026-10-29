#!/usr/bin/env python3
"""Test installation under temporary paths and unique registration keys."""

import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import subprocess
import shutil
import tempfile
import time
import uuid
import winreg


def run(binary, compiler, user_data=False):
    repo = Path(__file__).resolve().parent.parent
    if user_data and os.environ.get("CI") != "true":
        raise RuntimeError("The normal AppData test requires a disposable CI account")
    with tempfile.TemporaryDirectory(prefix="ludusavi-install-test-") as temporary:
        root = Path(temporary)
        identity = str(uuid.uuid4())
        registry = rf"Software\Microsoft\Windows\CurrentVersion\App Paths\ludusavi-test-{identity}.exe"
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
                str(compiler), f"/DBinary={binary}", f"/DVersion={version}", "/DArchitecture=x64",
                f"/DOutput={root}", str(test_script),
            ], check=True, stdout=subprocess.DEVNULL)
        install = root / "installed"
        configuration = root / "configuration"
        if user_data:
            appdata = ctypes.create_unicode_buffer(32768)
            assert ctypes.windll.shell32.SHGetFolderPathW(None, 26, None, 0, appdata) == 0
            configuration = Path(appdata.value) / "ludusavi"
            assert not configuration.exists(), "The test account already has Ludusavi data"
        configuration.mkdir()
        isolated = [str(binary), "--config", str(root / "defaults"), "--no-manifest-update"]
        config = json.loads(subprocess.check_output(isolated + ["config", "show", "--api"], text=True))
        config["backup"]["path"] = str(root / "backups")
        config["release"]["check"] = False
        sentinel = configuration / "config.yaml"
        sentinel.write_text(json.dumps(config), encoding="utf-8")
        expected = sentinel.read_bytes()
        try:
            for version in ["0.31.0", "0.31.1"]:
                subprocess.run([
                    str(root / f"ludusavi-{version}-x64-setup.exe"), "/CURRENTUSER", "/VERYSILENT",
                    "/SUPPRESSMSGBOXES", "/NORESTART", f"/DIR={install}", f"/GROUP=Ludusavi Test {identity}",
                ], check=True)
                assert hashlib.sha256(binary.read_bytes()).digest() == hashlib.sha256((install / "ludusavi.exe").read_bytes()).digest()
                assert sentinel.read_bytes() == expected, "Installation or upgrade changed configuration"
                command = [str(install / "ludusavi.exe"), "--no-manifest-update"]
                if not user_data:
                    command += ["--config", str(configuration)]
                current = json.loads(subprocess.check_output(command + ["config", "show", "--api"], text=True))
                assert current == config, "Installed app used the wrong configuration or changed saved settings"
                expected = sentinel.read_bytes()
            with winreg.OpenKey(winreg.HKEY_CURRENT_USER, registry) as key:
                assert winreg.QueryValue(key, None) == str(install / "ludusavi.exe")
            subprocess.run([str(install / "ludusavi.exe"), "--version"], check=True)
            subprocess.run([str(install / "unins000.exe"), "/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"], check=True)
            assert not (install / "ludusavi.exe").exists()
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
            if user_data:
                shutil.rmtree(configuration)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("compiler", type=Path)
    parser.add_argument("--user-data", action="store_true", help="Check the normal AppData folder on a disposable CI account")
    args = parser.parse_args()
    run(args.binary.resolve(strict=True), args.compiler.resolve(strict=True), args.user_data)
