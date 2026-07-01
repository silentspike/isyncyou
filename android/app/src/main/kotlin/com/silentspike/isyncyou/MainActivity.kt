package com.silentspike.isyncyou

import android.Manifest
import android.app.Activity
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.view.KeyEvent
import android.webkit.CookieManager
import android.webkit.WebResourceRequest
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

    private companion object {
        const val TAG = "iSyncYou"
    }

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
        web.webViewClient = object : WebViewClient() {
            // The local UI (127.0.0.1) stays in the WebView; hand any external
            // navigation — e.g. the device-code sign-in at login.live.com — to the
            // system browser so the auth page never takes over the app's own UI
            // (#89; aligns with RFC 8252: use the system browser for OAuth).
            override fun shouldOverrideUrlLoading(
                view: WebView,
                request: WebResourceRequest,
            ): Boolean {
                val url = request.url
                // Only http/https is ever allowed. Anything else (intent:, javascript:,
                // file:, data:, tel:, custom schemes) is refused outright — defense in
                // depth so a hostile link can't drive the WebView into a local scheme.
                val scheme = url.scheme?.lowercase()
                if (scheme != "http" && scheme != "https") return true
                // The local UI stays in the WebView.
                val host = url.host
                if (host == "127.0.0.1" || host == "localhost") return false
                // External http(s) — e.g. the device-code sign-in at login.live.com —
                // goes to the system browser, never inside the app's own UI.
                return try {
                    startActivity(Intent(Intent.ACTION_VIEW, url))
                    true
                } catch (_: Exception) {
                    // Can't hand off → refuse rather than load the external URL in-app.
                    true
                }
            }

            // Emit a stable signal once the local shell has rendered. Used by the CI
            // emulator smoke (REQ-AND-004) to assert the WebView loaded the embedded
            // UI, and handy for on-device diagnostics.
            override fun onPageFinished(view: WebView, url: String) {
                if (url.startsWith("http://127.0.0.1") || url.startsWith("http://localhost")) {
                    android.util.Log.i(TAG, "shell loaded: $url")
                }
            }
        }
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
        // A Firebase-less build (no google-services.json — the documented token-free
        // assembleDebug path) has no default FirebaseApp, so FirebaseMessaging.getInstance()
        // would throw and crash onCreate. Guard on it: without Firebase the app still runs
        // fully (push is simply unavailable), which is what the Firebase-less build intends.
        if (com.google.firebase.FirebaseApp.getApps(this).isNotEmpty()) {
            com.google.firebase.messaging.FirebaseMessaging.getInstance().token
                .addOnCompleteListener { t ->
                    if (t.isSuccessful) {
                        fcmToken = t.result
                        // Persist so a later rotation (onNewToken) and the web UI's push
                        // registration read a single, current source.
                        IsyncMessagingService.saveToken(this, t.result)
                    }
                }
        }

        // Start the embedded engine off the UI thread (it touches the filesystem and
        // binds a socket), then load the local UI on the UI thread once it's up.
        val filesPath = filesDir.absolutePath
        Thread {
            // Catch Throwable: a failed System.loadLibrary (UnsatisfiedLinkError /
            // ExceptionInInitializerError) or a native panic would otherwise kill this
            // thread silently — onEngineReady would never run and the UI would hang on a
            // blank WebView with no log. Logging the start + the outcome makes engine
            // failures visible (the CI emulator smoke and on-device diagnostics rely on it).
            try {
                android.util.Log.i(TAG, "engine thread: calling nativeStart")
                val port = NativeEngine.nativeStart(filesPath)
                val token = if (port > 0) NativeEngine.nativeSessionToken() else ""
                android.util.Log.i(TAG, "engine thread: nativeStart returned port=$port")
                runOnUiThread { onEngineReady(port, token) }
            } catch (t: Throwable) {
                android.util.Log.e(TAG, "engine thread crashed starting the native engine", t)
                runOnUiThread { onEngineReady(-1, "") }
            }
        }.start()
    }

    /** Wire the session token into the WebView and load the local UI (UI thread). */
    private fun onEngineReady(port: Int, token: String) {
        if (port <= 0) {
            android.util.Log.e(TAG, "embedded engine failed to start")
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
        web.addJavascriptInterface(NavBridge(), "AndroidNav")
        CookieManager.getInstance().apply {
            setAcceptCookie(true)
            setCookie("$origin/", "isy_session=$token; Path=/")
            // setCookie is async; flush() persists it synchronously so the very first
            // subresource requests (iframe/img/EventSource) already carry the session
            // cookie and don't 401 on the initial load (#89 P1).
            flush()
        }
        // nativeStart is idempotent, so on Activity recreation the port is the same
        // origin and the UI simply reloads.
        android.util.Log.i(TAG, "engine bound 127.0.0.1:$port")
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
        fun fcmToken(): String {
            // Prefer the persisted token (kept current across rotations by onNewToken),
            // falling back to the value fetched at startup.
            val persisted = IsyncMessagingService.currentToken(this@MainActivity)
            return if (persisted.isNotEmpty()) persisted else (fcmToken ?: "")
        }
    }

    /**
     * JS bridge: open an external URL by handing the RAW string straight to the system browser.
     * Using `location.href` instead would route the URL through the WebView's own navigation,
     * which re-parses/normalises it — mangling the percent-encoded `redirect_uri`/`scope` and
     * following the `claude.com/cai`→`claude.ai` redirect in-WebView before hand-off. claude.ai
     * then rejects the consent submit with "Invalid request format". `Uri.parse` on the raw
     * string preserves the exact encoding, matching a direct `am start`/`xdg-open` (verified
     * on-device 2026-07-01: direct open completes the consent, WebView `location.href` fails).
     */
    private inner class NavBridge {
        @android.webkit.JavascriptInterface
        fun openExternal(url: String) {
            runOnUiThread {
                try {
                    startActivity(Intent(Intent.ACTION_VIEW, android.net.Uri.parse(url)))
                } catch (_: Exception) {
                }
            }
        }

        /**
         * Start the OAuth network-guard foreground service just before opening the browser,
         * so the loopback token exchange survives the app being backgrounded during sign-in
         * (see [OAuthGuardService]). Must be called while the Activity is still foreground —
         * it is, since the user just tapped "Connect" — so the Android 14 background-FGS-start
         * restriction does not apply.
         */
        @android.webkit.JavascriptInterface
        fun beginNetworkGuard() {
            runOnUiThread { OAuthGuardService.start(this@MainActivity) }
        }

        /** Stop the guard once sign-in completes, times out, or is cancelled. */
        @android.webkit.JavascriptInterface
        fun endNetworkGuard() {
            runOnUiThread { OAuthGuardService.stop(this@MainActivity) }
        }
    }

    /** Hardware/gesture back navigates WebView history before leaving the app. */
    override fun onKeyDown(keyCode: Int, event: KeyEvent?): Boolean {
        if (keyCode == KeyEvent.KEYCODE_BACK && web.canGoBack()) {
            web.goBack()
            return true
        }
        return super.onKeyDown(keyCode, event)
    }

    /** Safety net: never leak the sign-in network guard past the Activity's life. */
    override fun onDestroy() {
        OAuthGuardService.stop(this)
        super.onDestroy()
    }
}
