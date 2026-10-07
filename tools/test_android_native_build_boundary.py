import pathlib
import shutil
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]


class AndroidNativeBuildBoundaryTests(unittest.TestCase):
    def test_remote_v2_invocation_and_manifest_bind_both_abis_to_api34(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            (root / "tools").mkdir()
            (root / "android/app/src/main").mkdir(parents=True)
            script = root / "tools/build-android-native.sh"
            shutil.copy2(ROOT / "tools/build-android-native.sh", script)
            (root / "rust-toolchain.toml").write_text('[toolchain]\nchannel = "1.99.0"\n')
            binaries = root / "bin"
            binaries.mkdir()
            fixtures = {
                "git": '#!/bin/sh\ncase "$1" in status) exit 0;; rev-parse) printf "%040d\\n" 1;; *) exit 2;; esac\n',
                "cargo": '''#!/bin/bash
printf '%s\\n' "$@" >> calls.txt
target=''
while (( $# )); do
  if [[ $1 == --target ]]; then target=$2; break; fi
  shift
done
[[ $target == aarch64-linux-android || $target == x86_64-linux-android ]] || exit 2
mkdir -p "target/$target/release"
printf ELF > "target/$target/release/libisyncyou_mobile.so"
''',
                "llvm-readelf": '#!/bin/sh\nprintf "LOAD 0 0 0 0 0 R 0x4000\\n"\n',
            }
            for name, content in fixtures.items():
                executable = binaries / name
                executable.write_text(content)
                executable.chmod(0o700)
            result = subprocess.run(
                ["bash", str(script)], cwd=root, capture_output=True, text=True,
                env={"PATH": str(binaries) + ":/usr/bin:/bin", "HOME": str(root),
                     "ISY_ANDROID_ABIS": "arm64-v8a,x86_64"},
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            calls = (root / "calls.txt").read_text().splitlines()
            self.assertEqual(calls.count("android-r30"), 2)
            self.assertEqual(calls.count("1.99.0"), 2)
            self.assertFalse(any("RUSTUP_TOOLCHAIN=" in argument for argument in calls))
            self.assertFalse(any("LINKER=" in argument for argument in calls))
            manifest = dict(line.split("=", 1) for line in
                            (root / "android/app/src/main/jniLibs/isyncyou-native.properties").read_text().splitlines())
            self.assertEqual(manifest["android_api"], "34")
            self.assertEqual(manifest["ndk_version"], "30.0.16248370")
            self.assertEqual(manifest["rust_toolchain"], "1.99.0")
            self.assertEqual(manifest["abis"], "arm64-v8a,x86_64")
            self.assertIn("sha256.arm64-v8a", manifest)
            self.assertIn("sha256.x86_64", manifest)

    def test_gradle_never_starts_local_rust(self) -> None:
        source = (ROOT / "android/app/build.gradle.kts").read_text(encoding="utf-8")
        self.assertNotIn("cargoNdkBuild", source)
        self.assertNotIn("Exec::class", source)
        self.assertNotIn("commandLine(", source)
        self.assertNotIn('ProcessBuilder(listOf("cargo")', source)
        self.assertNotIn('ProcessBuilder(listOf("rustc")', source)
        self.assertIn('tasks.named("preBuild") { dependsOn(validateRemoteNativeArtifact) }', source)

    def test_native_builder_defaults_to_remote_and_guards_ci_backend(self) -> None:
        source = (ROOT / "tools/build-android-native.sh").read_text(encoding="utf-8")
        self.assertIn("BUILDER=${ISY_ANDROID_NATIVE_BUILDER:-remote}", source)
        self.assertIn("cargo remote --no-copy-lock", source)
        self.assertIn("the github-actions backend is forbidden outside GitHub Actions", source)
        self.assertIn("source_commit=", source)
        self.assertIn("sha256.%s=", source)

    def test_native_builder_requires_16k_elf_alignment(self) -> None:
        source = (ROOT / "tools/build-android-native.sh").read_text(encoding="utf-8")
        self.assertIn("max-page-size=16384", source)
        self.assertIn("common-page-size=16384", source)
        self.assertIn('RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }$ANDROID_PAGE_RUSTFLAGS"', source)
        self.assertIn('verify_elf_page_alignment "$source_library"', source)
        self.assertIn("alignment >= 0x4000", source)


if __name__ == "__main__":
    unittest.main()
