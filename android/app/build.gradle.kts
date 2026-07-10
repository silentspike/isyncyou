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

android {
    namespace = "com.silentspike.isyncyou"
    compileSdk = 34
    // Single source of truth for the NDK; cargoNdkBuild references android.ndkVersion.
    // AGP fetches/validates this exact NDK, so the cross-compile is reproducible.
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
    // Firebase Cloud Messaging via the BoM (#575) — version-aligned, messaging only.
    implementation(platform("com.google.firebase:firebase-bom:33.7.0"))
    implementation("com.google.firebase:firebase-messaging")

    testImplementation("junit:junit:4.13.2")
    testImplementation("org.json:json:20240303")
    androidTestImplementation("androidx.test.ext:junit:1.2.1")
    androidTestImplementation("androidx.test:runner:1.6.2")
}

// #89: build the embedded Rust engine (libisyncyou_mobile.so) with cargo-ndk into
// app/src/main/jniLibs before assembling the APK. The Rust workspace is the parent
// of android/. The cargo binary, MSRV toolchain and NDK are overridable via env for
// CI reproducibility; defaults match the documented local setup (cargo via rustup so
// `+1.95.0` resolves, NDK r27d pinned by android.ndkVersion above).
val cargoNdkBuild by tasks.registering(Exec::class) {
    workingDir = rootProject.projectDir.parentFile
    val home = System.getProperty("user.home")
    val cargo = System.getenv("CARGO") ?: "$home/.cargo/bin/cargo"
    val toolchain = System.getenv("ISY_RUST_TOOLCHAIN") ?: "1.95.0"
    environment("ANDROID_NDK_HOME", "${android.sdkDirectory}/ndk/${android.ndkVersion}")
    // One -t per ABI; cargo-ndk maps arm64-v8a -> aarch64-linux-android and
    // x86_64 -> x86_64-linux-android, building each requested target's .so.
    val targetFlags = androidAbis.flatMap { listOf("-t", it) }
    // Extra cargo features are reserved for explicit local/test builds (for example the
    // #627-only agent-subscription-experimental local CLI fallback/capture surface).
    // The #623 product Claude/Codex app-OAuth provider path is part of the Rust mobile
    // default feature set, so it must not depend on ISY_CARGO_FEATURES.
    val extraFeatures = System.getenv("ISY_CARGO_FEATURES")
    val featureFlags = if (!extraFeatures.isNullOrBlank()) listOf("--features", extraFeatures) else emptyList()
    commandLine(
        listOf(cargo, "+$toolchain", "ndk") + targetFlags +
            listOf("-o", "android/app/src/main/jniLibs", "build", "-p", "isyncyou-mobile", "--release") + featureFlags,
    )
}
tasks.named("preBuild") { dependsOn(cargoNdkBuild) }
