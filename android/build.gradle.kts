// Root build file. Plugin versions are declared here (apply false) and applied in
// the :app module. The google-services plugin (Firebase/FCM, story 2 / #575) is
// added here when that story lands.
plugins {
    id("com.android.application") version "8.5.2" apply false
    id("org.jetbrains.kotlin.android") version "1.9.24" apply false
    // Firebase google-services plugin (FCM, story 2 / #575) — applied in :app.
    id("com.google.gms.google-services") version "4.4.2" apply false
}
