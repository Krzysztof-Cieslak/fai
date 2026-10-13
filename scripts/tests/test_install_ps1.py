"""Exercise the real installer archive validator in PowerShell 7 and Windows PowerShell."""

from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
import warnings
import zipfile

SCRIPTS = Path(__file__).resolve().parents[1]


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
        return subprocess.run([
            self.shell, "-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
            "-File", str(SCRIPTS / "tests/assert_installer_archive.ps1"),
            "-InstallerScript", str(SCRIPTS / "install/install.ps1"), "-ArchivePath", str(self.archive),
        ], capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=30)

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
