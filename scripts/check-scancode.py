#!/usr/bin/python3
"""Reject scan errors and unexpected notices, preserving known project metadata."""

import json
from pathlib import Path
import sys


POLICY = json.loads(
    (Path(__file__).resolve().parent.parent / ".github/scancode-policy.json").read_text()
)
EXPECTED = POLICY["expected_expressions"]
PACKAGING = POLICY["packaging_manifests"]
OWN_COPYRIGHT = POLICY["own_copyright"]


def unexpected_files(report, root_name):
    if not report.get("headers") or "files" not in report:
        raise ValueError("ScanCode report is missing headers or files")
    if any(header.get("errors") for header in report["headers"]):
        raise ValueError("ScanCode reported scan errors")
    if not any(entry.get("type") == "file" for entry in report["files"]):
        raise ValueError("ScanCode report contains no scanned files")
    hits = []
    for entry in report["files"]:
        if entry.get("scan_errors"):
            raise ValueError(f"ScanCode reported scan errors for {entry['path']}")
        if entry["type"] != "file":
            continue
        path = entry["path"].removeprefix(root_name + "/")
        expression = (entry.get("detected_license_expression") or "").casefold()
        copyrights = entry.get("copyrights") or []
        allowed = EXPECTED.get(path, set())
        # These two manifests contain our Debian package license-file reference
        # and our own copyright metadata, not copied third-party source.
        own_copyrights = path in PACKAGING and all(
            notice["copyright"].casefold() == OWN_COPYRIGHT for notice in copyrights
        )
        if (expression and expression not in allowed) or (copyrights and not own_copyrights):
            hits.append(entry["path"])
    return hits


def main():
    try:
        report = json.loads(Path(".scancode/scancode-report.json").read_text())
        hits = unexpected_files(report, Path.cwd().name)
        if hits:
            print("Unexpected license/copyright text found in:", file=sys.stderr)
            for path in hits:
                print(f"  - {path}", file=sys.stderr)
            return 1
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"Invalid or failed ScanCode report: {error}", file=sys.stderr)
        return 1
    print("No unexpected embedded license/copyright text found.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
