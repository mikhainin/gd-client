#!/usr/bin/python3
"""Test scanner bootstrap ordering and failures without installing packages."""

import os
from pathlib import Path
import shutil
import runpy
import subprocess
import tempfile
import unittest


SCRIPTS = Path(__file__).resolve().parent
checker = runpy.run_path(str(SCRIPTS / "check-scancode.py"))
unexpected_files = checker["unexpected_files"]

UV_STUB = """#!/bin/sh
set -eu
[ "${UV_HTTP_TIMEOUT:-}" = 30 ] || exit 29
printf '%s\\n' "$*" >> "$UV_CALLS"
case "$1" in
    venv)
        mkdir -p "$2/bin"
        printf '#!/bin/sh\\nexit 0\\n' > "$2/bin/python"
        chmod +x "$2/bin/python"
        ;;
    pip)
        if [ "${FAIL_INSTALL:-0}" != 0 ]; then
            echo "fixture installer failed" >&2
            exit 17
        fi
        cp "$SCANNER_STUB" .venv-scancode/bin/scancode
        chmod +x .venv-scancode/bin/scancode
        ;;
    *)
        echo "unexpected fake uv invocation" >&2
        exit 19
        ;;
esac
"""

SCANNER_STUB = """#!/bin/sh
set -eu
printf '%s\\n' "$@" > "$SCAN_ARGS"
if [ "${FAIL_SCAN:-0}" != 0 ]; then
    echo "fixture scan failed" >&2
    exit 23
fi
printf '{"files": []}\\n' > .scancode/scancode-report.json
"""


class BootstrapTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="scancode-bootstrap-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        scripts = self.root / "scripts"
        scripts.mkdir()
        for name in ("scancode-scan.sh", "scancode-requirements.txt"):
            shutil.copy2(SCRIPTS / name, scripts / name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.write_executable(self.bin / "uv", UV_STUB)
        self.scanner_stub = self.root / "scanner-stub"
        self.write_executable(self.scanner_stub, SCANNER_STUB)
        self.environment = {
            **os.environ,
            "PATH": f"{self.bin}:/usr/bin:/bin",
            "UV_CALLS": str(self.root / "uv-calls"),
            "SCAN_ARGS": str(self.root / "scan-args"),
            "SCANNER_STUB": str(self.scanner_stub),
            "FAIL_INSTALL": "0",
            "FAIL_SCAN": "0",
        }

    @staticmethod
    def write_executable(path, content):
        path.write_text(content)
        path.chmod(0o755)

    def existing_environment(self):
        bin_path = self.root / ".venv-scancode/bin"
        bin_path.mkdir(parents=True)
        self.write_executable(bin_path / "python", "#!/bin/sh\nexit 0\n")
        self.write_executable(
            bin_path / "scancode",
            "#!/bin/sh\necho 'stale scanner ran before repair' >&2\nexit 91\n",
        )

    def run_scan(self, *arguments):
        return subprocess.run(
            ["/bin/bash", str(self.root / "scripts/scancode-scan.sh"), *arguments],
            cwd=self.root.parent,
            env=self.environment,
            capture_output=True,
            text=True,
            timeout=5,
            check=False,
        )

    def uv_calls(self):
        return (self.root / "uv-calls").read_text().splitlines()

    def test_fresh_environment_uses_compatibility_requirements(self):
        result = self.run_scan()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.uv_calls(), [
            "venv .venv-scancode --python 3.12",
            "pip install --python .venv-scancode/bin/python --requirement scripts/scancode-requirements.txt",
        ])
        self.assertTrue((self.root / ".scancode/scancode-report.json").is_file())

    def test_existing_environment_is_reconciled_before_scanning(self):
        self.existing_environment()
        result = self.run_scan()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.uv_calls(), [
            "pip install --python .venv-scancode/bin/python --requirement scripts/scancode-requirements.txt",
        ])
        self.assertNotIn("stale scanner", result.stderr)
        result = self.run_scan()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(self.uv_calls()), 2)

    def test_install_failure_stops_before_scan(self):
        self.existing_environment()
        self.environment["FAIL_INSTALL"] = "1"
        result = self.run_scan()
        self.assertEqual(result.returncode, 17)
        self.assertIn("fixture installer failed", result.stderr)
        self.assertFalse((self.root / "scan-args").exists())
        self.assertNotIn("Report written", result.stdout)

    def test_scan_failure_is_not_reported_as_success(self):
        self.environment["FAIL_SCAN"] = "1"
        result = self.run_scan()
        self.assertEqual(result.returncode, 23)
        self.assertIn("fixture scan failed", result.stderr)
        self.assertNotIn("Report written", result.stdout)

    def test_scan_scope_and_extra_argument_boundaries_are_preserved(self):
        result = self.run_scan("--ignore", "folder with spaces/*")
        self.assertEqual(result.returncode, 0, result.stderr)
        arguments = (self.root / "scan-args").read_text().splitlines()
        self.assertEqual(arguments[:3], ["--license", "--copyright", "--info"])
        self.assertEqual(arguments[-7:], [
            "crates/", "scripts/", "Cargo.toml", "AGENTS.md", "README.md",
            "--ignore", "folder with spaces/*",
        ])

    def test_requirements_pin_the_verified_compatible_plugin_set(self):
        manifest = self.root / "scripts/scancode-requirements.txt"
        requirements = [
            line for line in manifest.read_text().splitlines()
            if line and not line.startswith("#")
        ]
        self.assertEqual(requirements, [
            "scancode-toolkit==32.5.0", "plugincode==32.0.0", "pluggy==1.6.0",
        ])


class ReportTests(unittest.TestCase):
    def report(self, path, expression=None, copyrights=None):
        return {"headers": [{}], "files": [{
            "path": "checkout/" + path, "type": "file",
            "detected_license_expression": expression, "copyrights": copyrights or [],
        }]}

    def test_known_project_metadata_is_accepted(self):
        for path, expressions in checker["EXPECTED"].items():
            for expression in expressions:
                notices = (
                    [{"copyright": checker["OWN_COPYRIGHT"]}]
                    if path in checker["PACKAGING"] else []
                )
                report = self.report(path, expression.upper(), notices)
                self.assertEqual(unexpected_files(report, "checkout"), [])

    def test_source_notices_and_other_metadata_are_rejected(self):
        for path, expression, notices in (
            ("crates/lib.rs", checker["EXPECTED"]["Cargo.toml"][0], []),
            ("Cargo.toml", "unexpected-license", []),
            ("README.md", None, [{"copyright": "copyright somebody else"}]),
            ("crates/gdrived/Cargo.toml", None, [{"copyright": "copyright somebody else"}]),
        ):
            report = self.report(path, expression, notices)
            self.assertEqual(unexpected_files(report, "checkout"), ["checkout/" + path])

    def test_scan_errors_and_incomplete_reports_fail(self):
        for report in (
            {}, {"headers": [{"errors": ["failed"]}], "files": []},
            {"headers": [{}], "files": [{"path": "x", "scan_errors": ["failed"]}]},
        ):
            with self.assertRaises(ValueError):
                unexpected_files(report, "checkout")


if __name__ == "__main__":
    unittest.main()
