import pathlib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]


class AndroidNativeBuildBoundaryTests(unittest.TestCase):
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


if __name__ == "__main__":
    unittest.main()
