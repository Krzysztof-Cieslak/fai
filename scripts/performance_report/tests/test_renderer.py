"""Two end-to-end presentation fixtures, with no compiler or benchmark process."""

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

PACKAGE = Path(__file__).resolve().parents[1]
FIXTURES = Path(__file__).resolve().parent / "fixtures"


class RendererTests(unittest.TestCase):
    def check_fixture(self, name):
        fixture = json.loads((FIXTURES / (name + ".json")).read_text(encoding="utf-8"))
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            data = root / "report-data.json"
            original = json.dumps(fixture["data"])
            data.write_text(original, encoding="utf-8")
            subprocess.run([sys.executable, str(PACKAGE), str(data), str(root / "reports"),
                            "--evidence-url", "https://example.test/evidence"], check=True)
            self.assertEqual(data.read_text(encoding="utf-8"), original)
            self.assertEqual({path.name for path in (root / "reports").iterdir()}, set(fixture["expected"]))
            for report, expected in fixture["expected"].items():
                text = (root / "reports" / report).read_text(encoding="utf-8")
                self.assertLess(len(text.encode()), 60000)
                self.assertEqual(text.count("<details>"), text.count("</details>"))
                for marker in expected["contains"]:
                    self.assertIn(marker, text, report)
                for marker in expected.get("absent", []):
                    self.assertNotIn(marker, text, report)

    def test_complete_report(self):
        self.check_fixture("complete")

    def test_omitted_comparisons(self):
        self.check_fixture("omitted")


if __name__ == "__main__":
    unittest.main()
