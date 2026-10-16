"""Keep the published-dependency example portable and aligned with onboarding."""

from pathlib import Path
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[2]
SAMPLE = ROOT / "samples/published-json"


class PublishedJsonSampleTests(unittest.TestCase):
    def test_sample_has_no_workspace_or_local_dependency_overrides(self):
        manifest = tomllib.loads((SAMPLE / "fai.toml").read_text())
        self.assertIs(manifest["package"]["publish"], False)
        self.assertNotIn("workspace", manifest)
        self.assertNotIn("overrides", manifest)
        self.assertEqual(manifest["dependencies"], {
            "json": {"git": "https://github.com/Krzysztof-Cieslak/fai.git",
                     "subdir": "packages/json", "version": "^0.1.0"},
        })

    def test_lock_pins_a_released_git_source_not_a_checkout_path(self):
        lock = tomllib.loads((SAMPLE / "fai.lock").read_text())
        dependencies = [package for package in lock["packages"] if "git" in package["logical"]]
        self.assertEqual(len(dependencies), 1)
        dependency = dependencies[0]
        self.assertEqual(dependency["version"], "0.1.0")
        self.assertIs(dependency["released"], True)
        self.assertEqual(dependency["source"]["subdir"], "packages/json")
        self.assertEqual(dependency["source"]["git"], dependency["logical"]["git"])
        self.assertNotIn("path", dependency["source"])
        self.assertRegex(dependency["source"]["rev"], r"^sha1:[0-9a-f]{40}$")
        self.assertRegex(dependency["checksum"], r"^blake3:[0-9a-f]{64}$")

    def test_lock_covers_every_declared_native_platform(self):
        manifest = tomllib.loads((SAMPLE / "fai.toml").read_text())
        lock = tomllib.loads((SAMPLE / "fai.lock").read_text())
        self.assertEqual(sorted(graph["target"] for graph in lock["graphs"]), sorted(manifest["lock-targets"]))
        self.assertEqual(len(lock["graphs"]), 6)

    def test_readme_uses_the_exact_executable_example(self):
        source = (SAMPLE / "src/Main.fai").read_text().strip()
        self.assertIn(f"```fai\n{source}\n```", (ROOT / "README.md").read_text())

    def test_readme_enters_the_project_before_running_commands(self):
        readme = (ROOT / "README.md").read_text()
        section = readme.split("### Use a published package", 1)[1].split("## Design ideas", 1)[0]
        self.assertIn("fai init hello-json\ncd hello-json\nfai add https://", section)
        self.assertNotIn("-C hello-json", section)
        self.assertIn("Replace `src/Main.fai`", section)
        self.assertIn("Run `fai run`", section)


if __name__ == "__main__":
    unittest.main()
