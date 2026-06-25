package com.silentspike.isyncyou

import android.Manifest
import android.app.Activity
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.view.KeyEvent
import android.webkit.CookieManager
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient

/**
 * iSyncYou Android client (#89) — a hardened WebView onto the iSyncYou engine that
 * runs **in this app process**. On launch the native library starts the embedded
 * loopback server (the real engine, live-companion profile) and this WebView loads
 * `http://127.0.0.1:<port>/`. No desktop daemon, no `adb reverse` — the phone is a
 * self-contained iSyncYou node over mobile data. A thin shell: all features live in
 * the web UI.
 */
class MainActivity : Activity() {

    private lateinit var web: WebView

    /** The device's FCM registration token (fetched async; read by the JS bridge). */
    @Volatile
    private var fcmToken: String? = null

    /** The embedded engine's session token — gates the loopback data API (#89 P1). */
    @Volatile
    private var sessionToken: String = ""

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        web = WebView(this)
        web.settings.apply {
            javaScriptEnabled = true
            domStorageEnabled = true
            loadWithOverviewMode = true
            useWideViewPort = false
            // Always fetch the current UI from the embedded server — never a stale
            // app.js/app.css (#79).
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
        com.google.firebase.messaging.FirebaseMessaging.getInstance().token
            .addOnCompleteListener { t -> if (t.isSuccessful) fcmToken = t.result }

        // Start the embedded engine off the UI thread (it touches the filesystem and
        // binds a socket), then load the local UI on the UI thread once it's up.
        val filesPath = filesDir.absolutePath
        Thread {
            val port = NativeEngine.nativeStart(filesPath)
            val token = if (port > 0) NativeEngine.nativeSessionToken() else ""
            runOnUiThread { onEngineReady(port, token) }
        }.start()
    }

    /** Wire the session token into the WebView and load the local UI (UI thread). */
    private fun onEngineReady(port: Int, token: String) {
        if (port <= 0) {
            web.loadData(
                "<html><body style='font-family:sans-serif;padding:2rem'>" +
                    "<h2>iSyncYou</h2><p>The local engine failed to start.</p></body></html>",
                "text/html",
                "utf-8",
            )
            return
        }
        sessionToken = token
        val origin = "http://127.0.0.1:$port"
        // Deliver the session token two ways (#89 P1): a JS bridge for the web UI's
        // fetch() X-Session-Token header, and a loopback cookie that auto-rides
        // iframe/img/EventSource subresource requests. Both are set out-of-band — the
        // token is never served in a static asset another app could read.
        web.addJavascriptInterface(SessionBridge(), "AndroidSession")
        web.addJavascriptInterface(PushBridge(), "AndroidPush")
        CookieManager.getInstance().apply {
            setAcceptCookie(true)
            setCookie("$origin/", "isy_session=$token; Path=/")
        }
        // nativeStart is idempotent, so on Activity recreation the port is the same
        // origin and the UI simply reloads.
        web.loadUrl("$origin/")
    }

    /** JS bridge: the web UI reads the session token to gate its loopback API calls. */
    private inner class SessionBridge {
        @android.webkit.JavascriptInterface
        fun token(): String = sessionToken
    }

    /** JS bridge: lets the web UI read the FCM token for push registration (#576). */
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
}
