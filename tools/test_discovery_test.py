#!/usr/bin/env python3
import tempfile
import unittest
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).parent))

from test_discovery import kotlin_test_exists


class TestDiscovery(unittest.TestCase):
    def test_kotlin_helper_without_test_annotation_is_not_evidence(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            test_dir = root / "android/app/src/test/kotlin/example"
            test_dir.mkdir(parents=True)
            (test_dir / "Sample.kt").write_text(
                """class Sample {\n fun helperProof() {}\n @Test\n fun actualProof() {}\n}\n""",
                encoding="utf-8",
            )
            self.assertFalse(kotlin_test_exists(root, "helperProof"))
            self.assertTrue(kotlin_test_exists(root, "actualProof"))

    def test_instrumentation_source_set_is_supported(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            test_dir = root / "android/app/src/androidTest/kotlin/example"
            test_dir.mkdir(parents=True)
            (test_dir / "Sample.kt").write_text(
                "class Sample { @Test fun deviceProof() {} }", encoding="utf-8"
            )
            self.assertTrue(kotlin_test_exists(root, "deviceProof"))


if __name__ == "__main__":
    unittest.main()
