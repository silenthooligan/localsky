"""Keep versioned connector exports synchronized during normal and release CI."""
import subprocess
import sys
from pathlib import Path
import unittest


class DocumentationExportTests(unittest.TestCase):
    def test_rule_count_excludes_nested_test_fixture_tuples(self):
        root = Path(__file__).resolve().parents[2]
        guide = (root/'docs/src/llms-full.txt').read_text(encoding='utf-8')
        # The catalog has 24 entries; the separately nested label test fixture
        # used to be counted as a 25th entry by substring matching.
        self.assertIn('The catalog contains 24 built-in rules.', guide)
        self.assertNotIn('The catalog contains 25 built-in rules.', guide)

    def test_checked_in_exports_match_the_current_guide_and_versions(self):
        root = Path(__file__).resolve().parents[2]
        result = subprocess.run(
            [sys.executable, str(root/'.github/scripts/build-doc-assets.py'), '--check'],
            cwd=root, capture_output=True, text=True,
        )
        self.assertEqual(result.returncode, 0, result.stdout+result.stderr)


if __name__ == '__main__':
    unittest.main()
