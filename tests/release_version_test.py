import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from release_version import version_for_run


class ReleaseVersionTest(unittest.TestCase):
    def test_run_number_advances_base_patch(self):
        self.assertEqual(version_for_run("0.1.0", 4), "0.1.4")
        self.assertEqual(version_for_run("1.2.3", 4), "1.2.7")

    def test_invalid_versions_and_run_numbers_are_rejected(self):
        for base in ("1.2", "v1.2.3", "1.2.3-rc1", "01.2.3"):
            with self.subTest(base=base), self.assertRaises(ValueError):
                version_for_run(base, 1)
        with self.assertRaises(ValueError):
            version_for_run("1.2.3", 0)


if __name__ == "__main__":
    unittest.main()
