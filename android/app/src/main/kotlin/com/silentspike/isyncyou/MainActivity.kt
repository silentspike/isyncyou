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
import android.webkit.WebResourceResponse
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient
import androidx.webkit.JavaScriptReplyProxy
import androidx.webkit.WebViewCompat
import androidx.webkit.WebViewFeature
import java.io.ByteArrayInputStream
import java.io.File
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Executors
import org.json.JSONObject

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

        /** The stable app origin the WebView loads from (#0A) — WebView's reserved
         *  virtual host. GET assets/subresources are served in-process via
         *  `shouldInterceptRequest`, so no loopback TCP port is exposed. */
        const val APP_HOST = "appassets.androidplatform.net"
        const val APP_ORIGIN = "https://$APP_HOST"
    }

    private lateinit var web: WebView

    /** The device's FCM registration token (fetched async; read by the JS bridge). */
    @Volatile
    private var fcmToken: String? = null

    /** The embedded engine's session token — gates the loopback data API (#89 P1). */
    @Volatile
    private var sessionToken: String = ""

    /** Forwarding threads for the in-process bridge (#0A): one per request/stream, so a
     *  blocking `nativeStreamNext` never stalls the UI thread or another request. */
    private val bridgeExecutor = Executors.newCachedThreadPool()

    /** JS stream id -> native stream id, for `unsub`/teardown (#0A). */
    private val bridgeStreams = ConcurrentHashMap<String, Long>()

    /** Latches true on the first bridge message so we log the live data path once (#0A). */
    private val bridgeSeen = java.util.concurrent.atomic.AtomicBoolean(false)

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
            // The app-origin UI stays in the WebView; hand any external navigation —
            // e.g. the device-code sign-in at login.live.com — to the system browser so
            // the auth page never takes over the app's own UI (#89; RFC 8252).
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
                // The app-origin UI (and, during the staged rollout, loopback) stays in
                // the WebView.
                val host = url.host
                if (host == APP_HOST || host == "127.0.0.1" || host == "localhost") return false
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

            // Serve every app-origin GET (the shell + any img/iframe/viewer the WebView
            // loads itself) in-process from the embedded engine (#0A) — no loopback TCP
            // port. POST/PATCH/DELETE ride the message bridge (they carry a body, which a
            // WebResourceRequest cannot expose), so they are left to the network stack and
            // never reach here. External / non-app-origin requests return null (normal
            // handling → shouldOverrideUrlLoading).
            override fun shouldInterceptRequest(
                view: WebView,
                request: WebResourceRequest,
            ): WebResourceResponse? {
                val url = request.url
                if (url.host != APP_HOST) return null
                if (!request.method.equals("GET", ignoreCase = true)) return null
                val path = (url.encodedPath ?: "/") + (url.encodedQuery?.let { "?$it" } ?: "")
                // The session cookie auto-rides subresources; read it for the engine gate.
                val cookie = CookieManager.getInstance().getCookie(url.toString()) ?: ""
                return try {
                    decodeAssetResponse(NativeEngine.nativeAssetRequest(path, cookie))
                } catch (e: Exception) {
                    android.util.Log.w(TAG, "asset serve failed for ${url.encodedPath}", e)
                    null
                }
            }

            // Emit a stable signal once the shell has rendered. Used by the CI emulator
            // smoke (REQ-AND-004) and handy for on-device diagnostics.
            override fun onPageFinished(view: WebView, url: String) {
                if (url.startsWith(APP_ORIGIN) ||
                    url.startsWith("http://127.0.0.1") ||
                    url.startsWith("http://localhost")
                ) {
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
                // Install the at-rest body key from the Keystore BEFORE the engine touches
                // disk (#0B), so the first body write/read is already sealed.
                BodyKeyStore.getOrCreate(filesDir)?.let { r ->
                    if (r.justCreated) {
                        // First encrypted run: discard the pre-encryption plaintext CACHE (the
                        // store DB + body files) so it re-syncs sealed — but KEEP the auth
                        // token (also under archive/, `.isyncyou-token*`) so the user stays
                        // signed in. The cache is reproducible; the token is not.
                        File(filesDir, "archive").listFiles()?.forEach { f ->
                            if (!f.name.startsWith(".isyncyou-token")) f.deleteRecursively()
                        }
                        File(filesDir, "sync").deleteRecursively()
                        File(filesDir, "cache").deleteRecursively()
                        android.util.Log.i(TAG, "body encryption on: discarded plaintext cache (kept auth)")
                    }
                    NativeEngine.nativeSetBodyKey(r.keyId, r.key)
                    java.util.Arrays.fill(r.key, 0) // wipe the data key from the JVM heap
                }
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
        // The WebView loads from the stable **app origin** (#0A); GET assets/subresources
        // are served in-process by shouldInterceptRequest and the data path rides the
        // message bridge, so no loopback TCP port is exposed to other apps.
        web.addJavascriptInterface(SessionBridge(), "AndroidSession")
        web.addJavascriptInterface(PushBridge(), "AndroidPush")
        web.addJavascriptInterface(NavBridge(), "AndroidNav")
        // Origin-bound message bridge (#0A): the data path (api/post/streams) rides an
        // origin-bound WebMessageListener bound to the app origin — no other frame/origin
        // (and, with the no-script sandboxed viewers, no untrusted iframe) can reach it.
        setupBridge(APP_ORIGIN)
        CookieManager.getInstance().apply {
            setAcceptCookie(true)
            // The session cookie auto-rides app-origin subresources (iframe/img); it is set
            // out-of-band, never served in a static asset another app could read (#89 P1).
            setCookie("$APP_ORIGIN/", "isy_session=$token; Path=/")
            // setCookie is async; flush() persists it synchronously so the very first
            // subresource requests already carry the session cookie.
            flush()
        }
        // nativeStart is idempotent, so on Activity recreation the same engine reloads.
        android.util.Log.i(TAG, "engine ready (in-process), loading $APP_ORIGIN")
        web.loadUrl("$APP_ORIGIN/")
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

    /**
     * Register the origin-bound in-process message bridge `__isyBridge` (#0A). The JS side
     * (`app.js`) posts `{t:"req"|"sub"|"unsub",...}`; we forward to the embedded engine and
     * post replies back on the reply proxy. `allowedOriginRules` binds the object to the
     * engine's **exact** origin, so no other origin (and, with the sandboxed no-script
     * viewers, no untrusted iframe) can obtain it. No-op when the device's WebView lacks the
     * feature — app.js then falls back to loopback fetch, so the app still works.
     */
    private fun setupBridge(origin: String) {
        if (!WebViewFeature.isFeatureSupported(WebViewFeature.WEB_MESSAGE_LISTENER)) {
            android.util.Log.w(TAG, "WEB_MESSAGE_LISTENER unsupported; using loopback fetch")
            return
        }
        try {
            WebViewCompat.addWebMessageListener(web, "__isyBridge", setOf(origin)) {
                _, message, _, _, replyProxy ->
                (message.data)?.let { onBridgeMessage(it, replyProxy) }
            }
            android.util.Log.i(TAG, "bridge listener registered for $origin")
        } catch (e: Exception) {
            android.util.Log.w(TAG, "bridge registration failed; using loopback fetch", e)
        }
    }

    /** Dispatch one inbound bridge message off the UI thread (#0A). */
    private fun onBridgeMessage(data: String, reply: JavaScriptReplyProxy) {
        val t = try {
            JSONObject(data).optString("t")
        } catch (_: Exception) {
            return // not our envelope
        }
        // One-time signal that the WebView actually routes through the bridge (not the
        // loopback fetch fallback) — proves the in-process data path is live. No payload.
        if (bridgeSeen.compareAndSet(false, true)) {
            android.util.Log.i(TAG, "bridge active: first message t=$t")
        }
        when (t) {
            "req" -> bridgeExecutor.execute {
                // Rust returns the complete {t:"res",id,status,body} reply — post it verbatim.
                val resp = NativeEngine.nativeBridgeRequest(data)
                reply.postMessage(resp)
            }
            "sub" -> {
                val obj = JSONObject(data)
                val jsId = obj.optString("id")
                val path = obj.optString("path")
                if (jsId.isNotEmpty()) bridgeExecutor.execute { runBridgeStream(jsId, path, reply) }
            }
            "unsub" -> {
                val jsId = JSONObject(data).optString("id")
                bridgeStreams.remove(jsId)?.let { NativeEngine.nativeStreamClose(it) }
            }
        }
    }

    /** Drain one push stream, forwarding each event to the WebView until it ends (#0A). */
    private fun runBridgeStream(jsId: String, path: String, reply: JavaScriptReplyProxy) {
        val nativeId = NativeEngine.nativeStreamOpen(path, sessionToken)
        if (nativeId <= 0L) {
            reply.postMessage("{\"t\":\"end\",\"id\":\"$jsId\"}")
            return
        }
        bridgeStreams[jsId] = nativeId
        try {
            // Keep forwarding until the stream ends (empty) or an unsub removed our mapping.
            while (bridgeStreams[jsId] == nativeId) {
                val ev = NativeEngine.nativeStreamNext(nativeId)
                if (ev.isEmpty()) break
                // ev is a JSON {event,data} object — embed it as the `ev` field.
                reply.postMessage("{\"t\":\"evt\",\"id\":\"$jsId\",\"ev\":$ev}")
            }
        } finally {
            bridgeStreams.remove(jsId)
            NativeEngine.nativeStreamClose(nativeId)
            reply.postMessage("{\"t\":\"end\",\"id\":\"$jsId\"}")
        }
    }

    /**
     * Decode the framed bytes from [NativeEngine.nativeAssetRequest] (#0A) into a
     * WebResourceResponse: `[status:u16][ct_len:u16][content_type][hdr_len:u16][headers][body]`.
     * Preserves the engine's status, content-type (mime + charset) and extra response
     * headers (e.g. a viewer's Content-Security-Policy), plus `nosniff`.
     */
    private fun decodeAssetResponse(framed: ByteArray): WebResourceResponse {
        fun u16(i: Int) = ((framed[i].toInt() and 0xff) shl 8) or (framed[i + 1].toInt() and 0xff)
        if (framed.size < 6) {
            return WebResourceResponse(
                "text/plain", "utf-8", 503, "Unavailable",
                emptyMap(), ByteArrayInputStream(ByteArray(0)),
            )
        }
        val status = u16(0)
        val ctLen = u16(2)
        val ctFull = String(framed, 4, ctLen, Charsets.UTF_8)
        val hdrOff = 4 + ctLen
        val hdrLen = u16(hdrOff)
        val hdrsRaw = String(framed, hdrOff + 2, hdrLen, Charsets.UTF_8)
        val body = framed.copyOfRange(hdrOff + 2 + hdrLen, framed.size)

        val mime = ctFull.substringBefore(";").trim().ifEmpty { "application/octet-stream" }
        val enc = Regex("charset=([^;]+)", RegexOption.IGNORE_CASE)
            .find(ctFull)?.groupValues?.get(1)?.trim()
        val headers = HashMap<String, String>()
        headers["X-Content-Type-Options"] = "nosniff"
        hdrsRaw.split("\r\n").forEach { line ->
            val i = line.indexOf(':')
            if (i > 0) headers[line.substring(0, i).trim()] = line.substring(i + 1).trim()
        }
        val reason = if (status in 200..299) "OK" else "Status $status"
        return WebResourceResponse(mime, enc, status, reason, headers, ByteArrayInputStream(body))
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
