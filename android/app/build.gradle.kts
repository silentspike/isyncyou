import java.security.MessageDigest
import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

// Firebase google-services plugin (FCM, #575): processes app/google-services.json.
// Applied ONLY when that file is present — it is user-supplied + gitignored
// (android/.gitignore), so a fresh CI checkout has no Firebase config. Without this
// guard `:app:processDebugGoogleServices` fails on CI; with it a token-free
// assembleDebug builds Firebase-less (firebase-messaging still compiles; only the
// runtime google_app_id resource is absent), while a local checkout that has the file
// keeps FCM fully wired. (The plugin is on the classpath via the root build's
// `apply false`; it hooks the Android variants through afterEvaluate.)
if (file("google-services.json").exists()) {
    apply(plugin = "com.google.gms.google-services")
}

// Release signing reads android/signing.properties (user-supplied, gitignored) —
// REQ-AND-003 / story 4 (#577). Absent => no release signing config (debug only).
val signingProps = rootProject.file("signing.properties")
val hasReleaseSigning = signingProps.exists()
val releaseProps = Properties().apply {
    if (hasReleaseSigning) signingProps.inputStream().use { load(it) }
}

// REQ-AND-003: when release signing is REQUIRED (the CI release path sets
// ISY_REQUIRE_RELEASE_SIGNING=1), a missing keystore or an incomplete
// signing.properties must fail the build LOUDLY — never silently produce an
// unsigned release APK. Local dev builds (the env unset) keep the unsigned-release
// convenience.
if (System.getenv("ISY_REQUIRE_RELEASE_SIGNING") == "1") {
    val problem: String? = if (!hasReleaseSigning) {
        "android/signing.properties is absent"
    } else {
        val empties = listOf("storeFile", "keyAlias", "storePassword", "keyPassword")
            .filter { releaseProps.getProperty(it).isNullOrBlank() }
        if (empties.isNotEmpty()) {
            empties.joinToString(", ") { "$it is empty" }
        } else {
            val ks = rootProject.file(releaseProps.getProperty("storeFile"))
            if (!ks.exists()) "keystore file ${ks.path} does not exist" else null
        }
    }
    if (problem != null) {
        throw GradleException(
            "Release signing is required (ISY_REQUIRE_RELEASE_SIGNING=1) but $problem. " +
                "Provide the release keystore and signing.properties — in CI these come " +
                "from the ANDROID_KEYSTORE_B64 / ANDROID_KEYSTORE_PASSWORD / " +
                "ANDROID_KEY_ALIAS / ANDROID_KEY_PASSWORD secrets.",
        )
    }
}

// The shipped APK is arm64-only (#89). CI can override ISY_ANDROID_ABIS to build an
// x86_64 variant for the KVM-accelerated emulator smoke (REQ-AND-004) — the release
// build leaves it unset and stays arm64-v8a.
val androidAbis = (System.getenv("ISY_ANDROID_ABIS") ?: "arm64-v8a")
    .split(",").map { it.trim() }.filter { it.isNotEmpty() }

// Bounded native evidence hooks only. Product/runtime features are defined in the
// mobile crate defaults and cannot be forwarded through the Android build environment.
val allowedCargoTestFeatures = setOf(
    "agent-session-kdf-bench",
    "agent-credential-store-self-test",
    "mobile-job-device-test-hooks",
    "agent-network-device-test-hooks",
    "agent-account-lifecycle-device-test-hooks",
)
val requestedCargoTestFeatures = System.getenv("ISY_CARGO_FEATURES")
    ?.takeUnless { it.isBlank() }
    ?.split(",")
    ?.map { it.trim() }
    ?.also { requested ->
        if (requested.any { it.isEmpty() }) {
            throw GradleException("ISY_CARGO_FEATURES contains an empty feature")
        }
        val duplicates = requested.groupingBy { it }.eachCount().filterValues { it > 1 }.keys
        if (duplicates.isNotEmpty()) {
            throw GradleException(
                "ISY_CARGO_FEATURES contains duplicate features: ${duplicates.sorted().joinToString(",")}",
            )
        }
        val unsupported = requested.filterNot { it in allowedCargoTestFeatures }
        if (unsupported.isNotEmpty()) {
            throw GradleException(
                "Unsupported ISY_CARGO_FEATURES value: ${unsupported.sorted().joinToString(",")}",
            )
        }
    }
    .orEmpty()

android {
    namespace = "com.silentspike.isyncyou"
    compileSdk = 34
    // Single source of truth for the NDK used by the separate native build step.
    // Gradle never invokes Cargo or rustc; it only packages a validated artifact.
    ndkVersion = System.getenv("ISY_NDK_VERSION") ?: "27.3.13750724"

    defaultConfig {
        applicationId = "com.silentspike.isyncyou"
        minSdk = 24
        targetSdk = 34
        versionCode = (System.getenv("ISY_VERSION_CODE") ?: "1").toInt()
        versionName = System.getenv("ISY_VERSION_NAME") ?: "0.1"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        // arm64 by default; ISY_ANDROID_ABIS lets CI build an x86_64 emulator variant.
        ndk {
            abiFilters += androidAbis
        }
    }

    if (hasReleaseSigning) {
        signingConfigs {
            create("release") {
                storeFile = file(releaseProps.getProperty("storeFile"))
                keyAlias = releaseProps.getProperty("keyAlias")
                storePassword = releaseProps.getProperty("storePassword")
                keyPassword = releaseProps.getProperty("keyPassword")
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
            if (hasReleaseSigning) signingConfig = signingConfigs.getByName("release")
        }
        debug {
            applicationIdSuffix = ".debug"
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    buildFeatures {
        buildConfig = true
    }
    kotlinOptions {
        jvmTarget = "17"
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.webkit:webkit:1.11.0")
    // Biometric per-action confirmation for destructive ops (#onedrive-mobile 0.6).
    // Pulls androidx.fragment; MainActivity is a FragmentActivity for BiometricPrompt.
    implementation("androidx.biometric:biometric:1.1.0")
    // WorkManager is the only production executor for durable mobile jobs (#626).
    implementation("androidx.work:work-runtime-ktx:2.9.1")
    // Firebase Cloud Messaging via the BoM (#575) — version-aligned, messaging only.
    implementation(platform("com.google.firebase:firebase-bom:33.7.0"))
    implementation("com.google.firebase:firebase-messaging")

    testImplementation("junit:junit:4.13.2")
    testImplementation("org.json:json:20240303")
    testImplementation("androidx.work:work-runtime-ktx:2.9.1")
    androidTestImplementation("androidx.test.ext:junit:1.2.1")
    androidTestImplementation("androidx.test:core:1.6.1")
    androidTestImplementation("androidx.test:runner:1.6.2")
}

fun sha256(file: File): String {
    val digest = MessageDigest.getInstance("SHA-256")
    file.inputStream().buffered().use { input ->
        val buffer = ByteArray(DEFAULT_BUFFER_SIZE)
        while (true) {
            val count = input.read(buffer)
            if (count < 0) break
            digest.update(buffer, 0, count)
        }
    }
    return digest.digest().joinToString("") { "%02x".format(it) }
}

fun gitOutput(vararg args: String): String {
    val process = ProcessBuilder(listOf("git") + args)
        .directory(rootProject.projectDir.parentFile)
        .redirectErrorStream(true)
        .start()
    val output = process.inputStream.bufferedReader().use { it.readText() }.trim()
    if (process.waitFor() != 0) {
        throw GradleException("Unable to validate the remote native artifact against Git: $output")
    }
    return output
}

// Rust compilation is deliberately outside Gradle. Local builds use
// tools/build-android-native.sh, whose default backend is cargo remote; GitHub Actions
// may opt into its runner-only backend. This task fails closed on missing, stale, or
// mismatched native output and never starts Cargo or rustc itself.
val validateRemoteNativeArtifact by tasks.registering {
    val nativeDir = file("src/main/jniLibs")
    val manifestFile = nativeDir.resolve("isyncyou-native.properties")
    inputs.files(manifestFile, androidAbis.map { nativeDir.resolve("$it/libisyncyou_mobile.so") })

    doLast {
        if (!manifestFile.isFile) {
            throw GradleException(
                "Remote native artifact is missing. Run tools/build-android-native.sh from the repository root.",
            )
        }

        val nativeInputs = listOf("Cargo.toml", "Cargo.lock", "crates", "gui/webui")
        val dirtyInputs = gitOutput("status", "--porcelain", "--untracked-files=all", "--", *nativeInputs.toTypedArray())
        if (dirtyInputs.isNotEmpty()) {
            throw GradleException(
                "Rust/WebUI inputs changed after the native artifact boundary. Commit them, then rebuild with " +
                    "tools/build-android-native.sh.",
            )
        }

        val manifest = Properties().apply {
            manifestFile.inputStream().use { load(it) }
        }
        val expectedCommit = gitOutput("rev-parse", "HEAD")
        val expectedAbis = androidAbis.sorted().joinToString(",")
        val expectedFeatures = requestedCargoTestFeatures.sorted().joinToString(",")
        val expectedNdk = android.ndkVersion

        val bindings = mapOf(
            "schema" to "1",
            "source_commit" to expectedCommit,
            "abis" to expectedAbis,
            "features" to expectedFeatures,
            "ndk_version" to expectedNdk,
        )
        bindings.forEach { (key, expected) ->
            val actual = manifest.getProperty(key)
            if (actual != expected) {
                throw GradleException(
                    "Remote native artifact binding '$key' is '$actual', expected '$expected'. " +
                        "Rebuild it with tools/build-android-native.sh.",
                )
            }
        }

        androidAbis.forEach { abi ->
            val library = nativeDir.resolve("$abi/libisyncyou_mobile.so")
            if (!library.isFile || library.length() == 0L) {
                throw GradleException("Remote native library is missing or empty for ABI '$abi'.")
            }
            val expectedHash = manifest.getProperty("sha256.$abi")
            val actualHash = sha256(library)
            if (expectedHash == null || !actualHash.equals(expectedHash, ignoreCase = true)) {
                throw GradleException("Remote native library hash mismatch for ABI '$abi'.")
            }
        }
    }
}

tasks.named("preBuild") { dependsOn(validateRemoteNativeArtifact) }
