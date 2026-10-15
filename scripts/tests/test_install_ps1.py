"""Exercise the real installer archive validator in PowerShell 7 and Windows PowerShell."""

from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest import mock
import warnings
import zipfile

SCRIPTS = Path(__file__).resolve().parents[1]
VALIDATOR_TIMEOUT_SECONDS = 120


def validate_archive(shell, archive):
    # A fresh Windows PowerShell/.NET host can take longer than 30 seconds to
    # initialize on hosted runners. Keep the whole invocation bounded and fatal
    # on timeout; archive validation errors must still reach the caller unchanged.
    return subprocess.run([
        shell, "-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
        "-File", str(SCRIPTS / "tests/assert_installer_archive.ps1"),
        "-InstallerScript", str(SCRIPTS / "install/install.ps1"), "-ArchivePath", str(archive),
    ], capture_output=True, text=True, encoding="utf-8", errors="replace",
       timeout=VALIDATOR_TIMEOUT_SECONDS)


class PowerShellInvocationTests(unittest.TestCase):
    def test_cold_start_has_a_bounded_two_minute_budget(self):
        with mock.patch.object(subprocess, "run") as run:
            validate_archive("powershell.exe", Path("installer.zip"))
        self.assertEqual(run.call_args.kwargs["timeout"], 120)

    def test_timeout_is_fatal_and_is_not_retried(self):
        error = subprocess.TimeoutExpired("powershell.exe", 120)
        with mock.patch.object(subprocess, "run", side_effect=error) as run:
            with self.assertRaises(subprocess.TimeoutExpired) as raised:
                validate_archive("powershell.exe", Path("installer.zip"))
        self.assertIs(raised.exception, error)
        run.assert_called_once()

    def test_validation_failure_is_returned_unchanged(self):
        result = subprocess.CompletedProcess([], 1, "", "invalid archive")
        with mock.patch.object(subprocess, "run", return_value=result) as run:
            self.assertIs(validate_archive("powershell.exe", Path("installer.zip")), result)
        run.assert_called_once()


class PowerShellArchiveTests(unittest.TestCase):
    executable = "pwsh"

    def setUp(self):
        self.shell = shutil.which(self.executable)
        if self.shell is None:
            self.skipTest(f"{self.executable} is not installed")
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.archive = Path(self.temporary.name) / "installer.zip"

    def validate(self, members):
        with warnings.catch_warnings(), zipfile.ZipFile(self.archive, "w") as archive:
            warnings.simplefilter("ignore", UserWarning)
            for name, mode in members:
                entry = zipfile.ZipInfo(name)
                entry.create_system = 3
                entry.external_attr = mode << 16
                archive.writestr(entry, b"fixture")
        return validate_archive(self.shell, self.archive)

    def test_release_members_use_ordinal_not_culture_dependent_order(self):
        result = self.validate([("LICENSE", 0o100644), ("bin/fai.exe", 0o100755), ("metadata.json", 0o100644)])
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_archive_entry_order_does_not_change_validation(self):
        result = self.validate([("metadata.json", 0o100644), ("bin/fai.exe", 0o100755), ("LICENSE", 0o100644)])
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_duplicate_members_are_rejected(self):
        result = self.validate([("LICENSE", 0o100644), ("bin/fai.exe", 0o100755), ("bin/fai.exe", 0o100755)])
        self.assertNotEqual(result.returncode, 0)

    def test_path_traversal_is_rejected(self):
        result = self.validate([("../LICENSE", 0o100644), ("bin/fai.exe", 0o100755), ("metadata.json", 0o100644)])
        self.assertNotEqual(result.returncode, 0)

    def test_member_case_is_significant(self):
        result = self.validate([("license", 0o100644), ("bin/fai.exe", 0o100755), ("metadata.json", 0o100644)])
        self.assertNotEqual(result.returncode, 0)

    def test_symbolic_links_are_rejected(self):
        result = self.validate([("LICENSE", 0o100644), ("bin/fai.exe", 0o120777), ("metadata.json", 0o100644)])
        self.assertNotEqual(result.returncode, 0)


class WindowsPowerShellArchiveTests(PowerShellArchiveTests):
    executable = "powershell.exe"


if __name__ == "__main__":
    unittest.main()
