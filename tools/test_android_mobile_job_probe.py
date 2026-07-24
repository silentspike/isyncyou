#!/usr/bin/env python3
import importlib.util
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("android-mobile-job-probe.py")
SPEC = importlib.util.spec_from_file_location("android_mobile_job_probe", MODULE_PATH)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ProbeTests(unittest.TestCase):
    def test_state_roundtrip_is_json_and_bounded(self) -> None:
        state = {"serial": "pixel", "airplane_mode": False, "validated_network": True}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "state.json"
            MODULE.write_state(path, state)
            self.assertEqual(MODULE.read_state(path), state)

    def test_unknown_boolean_is_not_converted_to_permissive_true(self) -> None:
        self.assertIsNone(MODULE.bool_setting("unknown"))


if __name__ == "__main__":
    unittest.main()
