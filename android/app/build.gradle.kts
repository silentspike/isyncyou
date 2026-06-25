import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    // Firebase google-services plugin (FCM, #575): processes app/google-services.json.
    id("com.google.gms.google-services")
}

// Release signing reads android/signing.properties (user-supplied, gitignored) —
// REQ-AND-003 / story 4 (#577). Absent => no release signing config (debug only).
val signingProps = rootProject.file("signing.properties")
val hasReleaseSigning = signingProps.exists()
val releaseProps = Properties().apply {
    if (hasReleaseSigning) signingProps.inputStream().use { load(it) }
}

android {
    namespace = "com.silentspike.isyncyou"
    compileSdk = 34

    defaultConfig {
        applicationId = "com.silentspike.isyncyou"
        minSdk = 24
        targetSdk = 34
        versionCode = (System.getenv("ISY_VERSION_CODE") ?: "1").toInt()
        versionName = System.getenv("ISY_VERSION_NAME") ?: "0.1"
        // The embedded Rust engine ships arm64 only for the first milestone (#89);
        // multi-arch is deferred hardening.
        ndk {
            abiFilters += "arm64-v8a"
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
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.webkit:webkit:1.11.0")
    // Firebase Cloud Messaging via the BoM (#575) — version-aligned, messaging only.
    implementation(platform("com.google.firebase:firebase-bom:33.7.0"))
    implementation("com.google.firebase:firebase-messaging")
}

// #89: build the embedded Rust engine (libisyncyou_mobile.so) with cargo-ndk into
// app/src/main/jniLibs before assembling the APK. The Rust workspace is the parent
// of android/. Pins the MSRV toolchain + the installed NDK; uses the rustup cargo
// proxy so `+1.95.0` resolves.
val cargoNdkBuild by tasks.registering(Exec::class) {
    workingDir = rootProject.projectDir.parentFile
    val home = System.getProperty("user.home")
    environment("ANDROID_NDK_HOME", "${android.sdkDirectory}/ndk/27.3.13750724")
    commandLine(
        "$home/.cargo/bin/cargo", "+1.95.0", "ndk",
        "-t", "arm64-v8a",
        "-o", "android/app/src/main/jniLibs",
        "build", "-p", "isyncyou-mobile", "--release",
    )
}
tasks.named("preBuild") { dependsOn(cargoNdkBuild) }
