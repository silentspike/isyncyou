#!/usr/bin/env python3
"""Boundary tests for the isolated Cargo Remote Android source snapshot."""
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).with_name('build-android-native.sh')

class SnapshotBoundary(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.base = Path(self.tmp.name)
        self.root = self.base / 'project'
        self.root.mkdir()
        for directory in ('tools', 'crates/mobile', 'bin/cli', 'gui/webui', 'android/app/src/main', 'fakebin', 'scratch'):
            (self.root / directory).mkdir(parents=True, exist_ok=True)
        shutil.copyfile(SCRIPT, self.root / 'tools/build-android-native.sh')
        for name in ('Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', 'crates/mobile/Cargo.toml', 'bin/cli/Cargo.toml', 'gui/webui/Cargo.toml'):
            (self.root / name).write_text('fixture\n')
        subprocess.run(['git', 'init', '-q', str(self.root)], check=True)
        subprocess.run(['git', '-C', str(self.root), 'config', 'user.email', 'test@example.invalid'], check=True)
        subprocess.run(['git', '-C', str(self.root), 'config', 'user.name', 'Test'], check=True)
        subprocess.run(['git', '-C', str(self.root), 'add', 'tools', 'Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', 'crates', 'bin', 'gui'], check=True)
        subprocess.run(['git', '-C', str(self.root), 'commit', '-qm', 'fixture'], check=True)
        self.head = subprocess.check_output(['git', '-C', str(self.root), 'rev-parse', 'HEAD'], text=True).strip()
        self.legacy = self.root / '.cargo-remote.toml'
        self.legacy.write_text('[[remote]]\nname = "legacy"\n')
        cargo = self.root / 'fakebin/cargo'
        cargo.write_text('''#!/usr/bin/env bash
set -eu
[[ "$PWD" != "$TEST_ROOT" ]]
grep -q '^profile = "android-r27d-api24"$' .cargo-remote.toml
[[ ! -e AGENTS.md ]]
[[ "$*" == *'--no-copy-lock'* && "$*" == *'--locked'* ]]
printf '%s' "$PWD" > "$TEST_ROOT/build-path"
if [[ ${MUTATE_INPUT:-} == yes ]]; then echo changed >> "$TEST_ROOT/crates/mobile/Cargo.toml"; fi
mkdir -p target/aarch64-linux-android/release
printf fixture-library > target/aarch64-linux-android/release/libisyncyou_mobile.so
''')
        cargo.chmod(0o755)
        readelf = self.root / 'fakebin/readelf'
        readelf.write_text('#!/bin/sh\nprintf "  LOAD 0 0 0 0 0 R 0x4000\\n"\n')
        readelf.chmod(0o755)
        shutil.copyfile(readelf, self.root / 'fakebin/llvm-readelf')
        (self.root / 'fakebin/llvm-readelf').chmod(0o755)
        self.env = {**os.environ, 'PATH': str(self.root / 'fakebin') + ':/usr/bin:/bin',
                    'TEST_ROOT': str(self.root), 'TMPDIR': str(self.root / 'scratch'),
                    'ISY_ANDROID_NATIVE_BUILDER': 'remote-v2-api24', 'ISY_RUST_TOOLCHAIN': '1.98.1'}

    def tearDown(self):
        self.tmp.cleanup()

    def run_script(self, **env):
        return subprocess.run(['bash', str(self.root / 'tools/build-android-native.sh')],
                              env={**self.env, **env}, text=True, capture_output=True)

    def test_snapshot_is_isolated_and_receipt_is_bound(self):
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.legacy.read_text(), '[[remote]]\nname = "legacy"\n')
        self.assertFalse(Path((self.root / 'build-path').read_text()).exists())
        stage = self.root / 'android/app/src/main/jniLibs'
        properties = (stage / 'isyncyou-native.properties').read_text()
        self.assertIn('source_commit=' + self.head + '\n', properties)
        self.assertIn('sha256.arm64-v8a=' + hashlib.sha256(b'fixture-library').hexdigest(), properties)
        self.assertIn('ndk_version=27.3.13750724\n', properties)

    def test_changed_native_input_fails_before_replacing_artifact(self):
        prior = self.root / 'android/app/src/main/jniLibs'
        prior.mkdir()
        (prior / 'keep').write_text('original')
        result = self.run_script(MUTATE_INPUT='yes')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('archived native inputs changed', result.stderr)
        self.assertEqual((prior / 'keep').read_text(), 'original')
        self.assertFalse(Path((self.root / 'build-path').read_text()).exists())
        self.assertEqual(self.legacy.read_text(), '[[remote]]\nname = "legacy"\n')

if __name__ == '__main__':
    unittest.main()
