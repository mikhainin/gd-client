#!/usr/bin/python3
"""Read-only Add/Cancel smoke test against an explicitly owned UI PID."""

import argparse
import json
from pathlib import Path
import subprocess
import sys
import time

from atspi_ui import UIError, parser as helper_parser, positive_float, positive_int


def smoke(pid, timeout, add_names):
    deadline = time.monotonic() + timeout
    helper = Path(__file__).with_name("atspi_ui.py")

    def call(*arguments):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise UIError("Overall smoke deadline expired.")
        budget = min(15, remaining)
        request = vars(helper_parser().parse_args([
            "--pid", str(pid), "--timeout", str(budget), *arguments,
        ]))
        try:
            result = subprocess.run(
                [sys.executable, str(helper), "--worker", json.dumps(request)],
                capture_output=True, text=True, timeout=budget, check=False,
            )
        except subprocess.TimeoutExpired as exc:
            raise UIError("Overall smoke deadline expired; inspect the UI before retrying.") from exc
        if result.returncode:
            raise UIError(result.stderr.strip() or result.stdout.strip())
        return json.loads(result.stdout)

    def showing_cancels(tree):
        return [
            node for node in tree["nodes"]
            if node["name"] == "Cancel" and node["states"]["showing"]
        ]

    selector = [arg for name in add_names for arg in ("--name", name)]
    call("check")
    if showing_cancels(call("tree")):
        raise UIError("A Cancel control is already showing. Start with no open dialogs.")
    call("find", *selector)
    call("press", *selector)
    call("wait", "--name", "Cancel", "--wait-seconds", "5")
    call("press", "--name", "Cancel")
    # Rendering/accessibility state may settle asynchronously after the press.
    while True:
        tree = call("tree")
        if not showing_cancels(tree):
            break
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise UIError("Cancel did not close the dialog before the smoke deadline.")
        time.sleep(min(0.2, remaining))
    call("find", *selector)
    return {"success": True, "pid": pid, "scenario": "Add folder → Cancel",
            "settings_saved": False}


def main():
    cli = argparse.ArgumentParser(description=__doc__)
    cli.add_argument("--pid", type=positive_int, required=True,
                     help="PID of an instance you launched and own")
    cli.add_argument("--timeout", type=positive_float, default=60)
    cli.add_argument("--add-name", action="append",
                     help="Exact Add button name; repeat for explicit variants")
    args = cli.parse_args()
    try:
        print(json.dumps(smoke(
            args.pid, args.timeout,
            args.add_name or ["Add folder", "Add folder…", "Add folder..."],
        )))
        return 0
    except Exception as exc:
        print(json.dumps({"error": str(exc)}), file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
