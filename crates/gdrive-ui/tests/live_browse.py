#!/usr/bin/env python3
"""Exercise the real debug UI through the skill, against a private mock daemon."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import time


ROOT = Path(__file__).resolve().parents[3]
HELPER = ROOT / ".github/skills/gdrive-ui-testing/gdrive-ui-testing"
FIXTURE = Path(__file__).with_name("mock_daemon.py")


def run(*command, deadline=10):
    result = subprocess.run(command, capture_output=True, text=True, timeout=deadline)
    if result.returncode:
        raise RuntimeError(
            f"Command failed ({result.returncode}): {' '.join(map(str, command))}\n"
            f"{result.stdout}\n{result.stderr}"
        )
    return result.stdout


def stop(process):
    if process is not None and process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=3)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifacts", type=Path, required=True)
    parser.add_argument("--platform", choices=("wayland", "xcb"))
    args = parser.parse_args()
    if os.environ.get("GDRIVE_UI_TEST_BUS") != "1":
        sys.exit("Run inside dbus-run-session with GDRIVE_UI_TEST_BUS=1")
    if not os.environ.get("AT_SPI_BUS_ADDRESS"):
        sys.exit("Pass the desktop AT_SPI_BUS_ADDRESS into the private D-Bus session")
    args.artifacts.mkdir(parents=True, exist_ok=True)
    daemon = app = None
    with (args.artifacts / "mock-daemon.log").open("w") as daemon_log, (
        args.artifacts / "mock-ui.log"
    ).open("w") as ui_log:
        try:
            daemon = subprocess.Popen(
                [sys.executable, str(FIXTURE)], stdout=daemon_log, stderr=subprocess.STDOUT
            )
            ready_deadline = time.monotonic() + 5
            while True:
                if daemon.poll() is not None:
                    raise RuntimeError("Mock daemon exited; inspect mock-daemon.log")
                owner = run(
                    "busctl", "--user", "--timeout=1s", "call",
                    "org.freedesktop.DBus", "/org/freedesktop/DBus",
                    "org.freedesktop.DBus", "NameHasOwner", "s", "org.gclient.GDrive1",
                    deadline=2,
                )
                if owner.strip() == "b true":
                    break
                if time.monotonic() >= ready_deadline:
                    raise RuntimeError("Mock daemon did not acquire its bus name")
                time.sleep(0.05)
            environment = dict(os.environ, QT_LINUX_ACCESSIBILITY_ALWAYS_ON="1")
            # Qt 6.4's direct-address path can emit accessibility activation too early.
            # The fixture answers GetAddress asynchronously on the private session bus.
            environment.pop("AT_SPI_BUS_ADDRESS")
            if args.platform:
                environment["QT_QPA_PLATFORM"] = args.platform
            app = subprocess.Popen(
                [str(ROOT / "target/debug/gdrive-ui")],
                cwd=ROOT, env=environment, stdout=ui_log, stderr=subprocess.STDOUT,
            )

            def helper(*arguments, deadline=10):
                return json.loads(run(
                    str(HELPER), "--pid", str(app.pid),
                    "--timeout", str(deadline), *arguments, deadline=deadline + 3,
                ))

            def press(name, ancestor=None):
                arguments = ["press", "--name", name]
                if ancestor:
                    arguments.extend(["--ancestor", ancestor])
                return helper(*arguments)

            def wait(name, deadline=10):
                return helper(
                    "wait", "--name", name, "--wait-seconds", str(deadline - 1),
                    deadline=deadline,
                )

            def wait_hidden(name, ancestor, deadline=3):
                end = time.monotonic() + deadline
                while True:
                    remaining = end - time.monotonic()
                    if remaining <= 0:
                        raise RuntimeError(f"{name} under {ancestor} is still showing")
                    nodes = helper("tree", deadline=min(3, remaining))["nodes"]
                    if not any(
                        node["name"] == name and ancestor in node["ancestors"]
                        and node["states"]["showing"] for node in nodes
                    ):
                        return
                    time.sleep(min(0.05, max(0, end - time.monotonic())))

            def mode(value):
                run(
                    "busctl", "--user", "--timeout=3s", "call",
                    "org.gclient.GDrive1", "/org/gclient/GDrive1",
                    "org.gclient.TestDrive", "SetMode", "s", value, deadline=4,
                )

            def authentication(method, value):
                run(
                    "busctl", "--user", "--timeout=3s", "call",
                    "org.gclient.GDrive1", "/org/gclient/GDrive1",
                    "org.gclient.TestDrive", method, "b", str(value).lower(), deadline=4,
                )

            wait("Connected to gdrived")
            print("Connected to isolated mock daemon", flush=True)
            wait("My Drive/Parent/Selected")
            wait("Drive path unavailable; reselect folder")
            wait("My Drive")
            nodes = helper("tree")["nodes"]
            if any(
                node["states"]["showing"]
                and any(value in node["name"] for value in ("opaque-folder-id", "legacy-opaque-id"))
                for node in nodes
            ):
                raise RuntimeError("Configured folder displayed a Drive ID instead of its path")
            print("Configured folders render saved/root paths and legacy placeholder without IDs", flush=True)
            wait("Sign out")
            authentication("PauseAuthenticationReads", True)
            authentication("SetAuthenticated", False)
            wait("Sign in\u2026", deadline=3)
            authentication("SetAuthenticated", True)
            wait("Sign out", deadline=3)
            authentication("PauseAuthenticationReads", False)
            time.sleep(0.3)
            helper("find", "--name", "Sign out")
            print("Authentication signals update UI with polling stalled; stale snapshots ignored", flush=True)
            press("Add folder")
            wait("Browse Drive folders")

            # Cancellation must work while the backend never sends a reply.
            press("Browse Drive folders")
            wait("Loading Drive folders...")
            start = time.monotonic()
            press("Cancel", "Select Drive folder")
            wait_hidden("Cancel", "Select Drive folder")
            cancel_seconds = time.monotonic() - start
            if cancel_seconds >= 3:
                raise RuntimeError(f"Cancel took {cancel_seconds:.2f}s")
            print(f"Canceled stalled listing in {cancel_seconds:.3f}s", flush=True)

            # Exercise the actual production 30-second deadline, not a shorter test proxy.
            start = time.monotonic()
            press("Browse Drive folders")
            wait("Loading Drive folders...")
            wait("Retry", deadline=34)
            timeout_seconds = time.monotonic() - start
            if not 28 <= timeout_seconds < 34:
                raise RuntimeError(f"Unexpected browse deadline: {timeout_seconds:.2f}s")

            mode("error")
            press("Retry")
            wait("Retry")
            nodes = helper("tree")["nodes"]
            if not any("Fixture Drive request timed out" in node["name"] for node in nodes):
                raise RuntimeError("Daemon error was not rendered in the Drive dialog")

            mode("success")
            press("Retry")
            wait("Open folder Projects")
            press("Open folder Projects")
            wait("No sub-folders here.")
            press("Select this folder")
            wait("My Drive/Projects")
            nodes = helper("tree")["nodes"]
            if any(
                node["name"] == "Drive folder id" and node["states"]["showing"]
                for node in nodes
            ):
                raise RuntimeError("Manual Drive ID entry is showing without enabling Advanced")

            # A canceled delayed child response must not overwrite a reopened root listing.
            press("Browse Drive folders")
            wait("Open folder Projects")
            mode("delay")
            press("Open folder Projects")
            wait("Loading Drive folders...")
            press("Cancel", "Select Drive folder")
            wait_hidden("Cancel", "Select Drive folder")
            mode("success")
            press("Browse Drive folders")
            wait("Open folder Projects")
            time.sleep(1.7)
            helper("find", "--name", "Open folder Projects")
            press("Cancel", "Select Drive folder")
            wait_hidden("Cancel", "Select Drive folder")
            press("Cancel", "Add sync folder")
            wait_hidden("Cancel", "Add sync folder")
            wait("Add folder")

            print(json.dumps({
                "success": True,
                "cancel_seconds": round(cancel_seconds, 3),
                "timeout_seconds": round(timeout_seconds, 3),
                "scenarios": [
                    "cancel stalled request", "production deadline", "error and retry",
                    "nested selection", "late reply after cancel/reopen",
                    "authentication signals with stalled polling", "stale authentication snapshots",
                ],
            }))
        finally:
            try:
                if app is not None and app.poll() is None:
                    with (args.artifacts / "final-accessibility-tree.log").open("w") as tree_log:
                        subprocess.run(
                            [str(HELPER), "--pid", str(app.pid),
                             "--timeout", "3", "tree"],
                            stdout=tree_log, stderr=subprocess.STDOUT, timeout=5,
                        )
            finally:
                try:
                    stop(app)
                finally:
                    stop(daemon)


if __name__ == "__main__":
    main()
