// Root build file. Plugin versions are declared here (apply false) and applied in
// the :app module. The google-services plugin (Firebase/FCM, story 2 / #575) is
// added here when that story lands.
buildscript {
    repositories {
        google()
        mavenCentral()
    }
    dependencies {
        classpath("org.jetbrains.kotlin:kotlin-gradle-plugin:2.4.20")
    }
}

plugins {
    id("com.android.application") version "9.4.1" apply false
    // Firebase google-services plugin (FCM, story 2 / #575) — applied in :app.
    id("com.google.gms.google-services") version "4.5.0" apply false
}
