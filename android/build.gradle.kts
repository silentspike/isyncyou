import org.gradle.api.artifacts.dsl.LockMode

// Root build file. Plugin versions are declared here (apply false) and applied in
// the :app module. The google-services plugin (Firebase/FCM, story 2 / #575) is
// added here when that story lands.
plugins {
    id("com.android.application") version "8.5.2" apply false
    // UnifiedPush connector 3.3.x publishes Kotlin 2.3 metadata.
    id("org.jetbrains.kotlin.android") version "2.3.0" apply false
    // Firebase google-services plugin (FCM, story 2 / #575) — applied in :app.
    id("com.google.gms.google-services") version "4.4.2" apply false
}

// Standard and NoGoogle resolve different graphs under the same Gradle
// configuration names, so each needs its own lock state. The root project has
// no project dependencies; plugin artifacts use dependency verification.
project(":app") {
    dependencyLocking {
        lockAllConfigurations()
        lockMode = LockMode.STRICT
        lockFile = if (providers.gradleProperty("isyncyouNoGoogle").orNull == "true") {
            file("gradle-nogoogle.lockfile")
        } else {
            file("gradle.lockfile")
        }
    }
}
