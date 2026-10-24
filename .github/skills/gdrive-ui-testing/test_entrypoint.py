"""Exercise the dedicated executable without a desktop or generic interpreter CLI."""

from pathlib import Path
import subprocess
import unittest


ENTRYPOINT = Path(__file__).with_name("gdrive-ui-testing")


class EntryPointTests(unittest.TestCase):
    def run_entrypoint(self, *arguments, cwd=None):
        return subprocess.run(
            [str(ENTRYPOINT), *arguments],
            cwd=cwd,
            capture_output=True,
            text=True,
            timeout=5,
            check=False,
        )

    def test_help_exposes_fixed_commands_from_any_directory(self):
        result = self.run_entrypoint("--help", cwd=ENTRYPOINT.parent.parent)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("smoke --pid PID, test, qml-test", result.stdout)
        self.assertIn("--pid", result.stdout)

    def test_smoke_dispatches_to_its_own_script(self):
        result = self.run_entrypoint("smoke", "--help")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--add-name", result.stdout)

    def test_generic_python_options_are_not_accepted(self):
        result = self.run_entrypoint("-c", "raise SystemExit(99)")
        self.assertEqual(result.returncode, 2)
        self.assertIn("usage:", result.stderr)

    def test_internal_worker_cannot_bypass_the_public_deadline(self):
        result = self.run_entrypoint("--worker", "{}")
        self.assertEqual(result.returncode, 2)
        self.assertIn("not a public command", result.stderr)

    def test_validation_cannot_be_redirected_to_other_code(self):
        for command in ("test", "qml-test"):
            with self.subTest(command=command):
                result = self.run_entrypoint(command, "--input", "/not/a/skill/fixture")
                self.assertEqual(result.returncode, 2)
                self.assertIn("Usage:", result.stderr)


if __name__ == "__main__":
    unittest.main()
