package com.silentspike.isyncyou

import android.Manifest
import android.app.Activity
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.view.KeyEvent
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient

/**
 * iSyncYou Android client — a hardened WebView onto the local iSyncYou daemon's
 * web UI. For on-device testing the daemon is reached over `adb reverse`
 * (localhost:8869 -> host); a real deployment points [SERVER_URL] at the daemon's
 * reachable address (LAN IP / NetBird VPN). A thin shell: all features live in the
 * web UI, so this stays small.
 */
class MainActivity : Activity() {

    private lateinit var web: WebView

    /** The device's FCM registration token (fetched async; read by the JS bridge). */
    @Volatile
    private var fcmToken: String? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        web = WebView(this)
        web.settings.apply {
            javaScriptEnabled = true
            domStorageEnabled = true
            loadWithOverviewMode = true
            useWideViewPort = false
            // Always fetch the current UI from the daemon — avoid serving a stale
            // app.js/app.css so changes always show (#79).
            cacheMode = WebSettings.LOAD_NO_CACHE
        }
        web.clearCache(true)
        // Keep navigation inside the WebView (don't hand off to Chrome).
        web.webViewClient = WebViewClient()
        setContentView(web)

        // FCM (#575): register the notification channel + request POST_NOTIFICATIONS
        // (Android 13+ needs a runtime grant before notifications can show).
        IsyncMessagingService.ensureChannel(this)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 1)
        }
        // Expose this device's FCM token to the web UI (#576), which registers it
        // with the daemon (the UI holds the daemon's capability token; the native
        // shell does not). The token is fetched async + cached.
        web.addJavascriptInterface(PushBridge(), "AndroidPush")
        com.google.firebase.messaging.FirebaseMessaging.getInstance().token
            .addOnCompleteListener { t -> if (t.isSuccessful) fcmToken = t.result }

        if (savedInstanceState != null) {
            web.restoreState(savedInstanceState)
        } else {
            web.loadUrl(SERVER_URL)
        }
    }

    /** JS bridge: lets the web UI read the FCM token to register it with the daemon. */
    private inner class PushBridge {
        @android.webkit.JavascriptInterface
        fun fcmToken(): String = fcmToken ?: ""
    }

    /** Hardware/gesture back navigates WebView history before leaving the app. */
    override fun onKeyDown(keyCode: Int, event: KeyEvent?): Boolean {
        if (keyCode == KeyEvent.KEYCODE_BACK && web.canGoBack()) {
            web.goBack()
            return true
        }
        return super.onKeyDown(keyCode, event)
    }

    override fun onSaveInstanceState(outState: Bundle) {
        super.onSaveInstanceState(outState)
        web.saveState(outState)
    }

    companion object {
        /** Default daemon URL. Works on a USB device via `adb reverse tcp:8869`. */
        private const val SERVER_URL = "http://localhost:8869/"
    }
}
