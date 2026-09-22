"""Keep versioned connector exports synchronized during normal and release CI."""
import subprocess
import sys
from pathlib import Path
import unittest


class DocumentationExportTests(unittest.TestCase):
    def test_checked_in_exports_match_the_current_guide_and_versions(self):
        root = Path(__file__).resolve().parents[2]
        result = subprocess.run(
            [sys.executable, str(root/'.github/scripts/build-doc-assets.py'), '--check'],
            cwd=root, capture_output=True, text=True,
        )
        self.assertEqual(result.returncode, 0, result.stdout+result.stderr)


if __name__ == '__main__':
    unittest.main()
