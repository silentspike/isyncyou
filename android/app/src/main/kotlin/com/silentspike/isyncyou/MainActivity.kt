package com.silentspike.isyncyou

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.net.ConnectivityManager
import android.os.BatteryManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.StatFs
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyPermanentlyInvalidatedException
import android.security.keystore.KeyProperties
import android.view.KeyEvent
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient
import androidx.biometric.BiometricManager
import androidx.biometric.BiometricPrompt
import androidx.core.content.ContextCompat
import androidx.fragment.app.FragmentActivity
import androidx.webkit.JavaScriptReplyProxy
import androidx.webkit.WebViewCompat
import androidx.webkit.WebViewFeature
import java.io.ByteArrayInputStream
import java.security.KeyStore
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Executors
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import org.json.JSONObject

/**
 * iSyncYou Android client (#89) — a hardened WebView onto the iSyncYou engine that
 * runs **in this app process**. On launch the Activity first preflights the
 * origin-bound WebMessage bridge, then starts the embedded engine and loads the
 * app-origin WebView shell. No desktop daemon and no mobile TCP fallback are part
 * of the default mobile data path.
 */
class MainActivity : FragmentActivity() {

    private companion object {
        const val TAG = "iSyncYou"
        const val BIO_TIMEOUT_MS = 120_000L

        /** The stable app origin the WebView loads from (#0A) — WebView's reserved
         *  virtual host. GET assets/subresources are served in-process via
         *  `shouldInterceptRequest`, so no mobile TCP data port is exposed. */
        const val APP_HOST = "appassets.androidplatform.net"
        const val APP_ORIGIN = "https://$APP_HOST"
    }

    private lateinit var web: WebView

    private val mainHandler = Handler(Looper.getMainLooper())

    /** The device's FCM registration token (fetched async; read by the JS bridge). */
    @Volatile
    private var fcmToken: String? = null

    /** The embedded engine's session token — held natively and never exposed to JS. */
    @Volatile
    private var sessionToken: String = ""

    /** Forwarding threads for the in-process bridge (#0A): one per request/stream, so a
     *  blocking `nativeStreamNext` never stalls the UI thread or another request. */
    private val bridgeExecutor = Executors.newCachedThreadPool()

    /** JS stream id -> native stream id, for `unsub`/teardown (#0A). */
    private val bridgeStreams = ConcurrentHashMap<String, Long>()

    /** Latches true on the first bridge message so we log the live data path once (#0A). */
    private val bridgeSeen = java.util.concurrent.atomic.AtomicBoolean(false)

    private val oauthGuards = OAuthGuardRegistry(
        onStart = { runOnUiThread { OAuthGuardService.start(this@MainActivity) } },
        onStop = { runOnUiThread { OAuthGuardService.stop(this@MainActivity) } },
    )

    private data class PendingBio(
        val prompt: BiometricPrompt,
        val timeout: Runnable,
    )

    private val bioPending = ConcurrentHashMap<String, PendingBio>()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        // Debuggable builds only: expose the WebView to chrome://inspect / CDP for
        // on-device debugging and verification. The FLAG_DEBUGGABLE bit is unset in release
        // builds, so this never fires there.
        if ((applicationInfo.flags and android.content.pm.ApplicationInfo.FLAG_DEBUGGABLE) != 0) {
            WebView.setWebContentsDebuggingEnabled(true)
        }
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
            // The app-origin UI stays in the WebView. Typed, exact auth/device-code
            // destinations are handed to the system browser. Everything else is consumed
            // fail-closed rather than loaded in-app.
            override fun shouldOverrideUrlLoading(
                view: WebView,
                request: WebResourceRequest,
            ): Boolean {
                val decision = ExternalUrlPolicy.classifyNavigationUrl(request.url.toString())
                return when (decision.action) {
                    NavigationAction.AppOrigin -> false
                    NavigationAction.ExternalBrowser -> {
                        try {
                            startActivity(Intent(Intent.ACTION_VIEW, request.url))
                        } catch (_: Exception) {
                            android.util.Log.w(TAG, "external auth navigation failed (${decision.reason})")
                        }
                        true
                    }
                    NavigationAction.Block -> true
                }
            }

            // Serve every app-origin GET (the shell + any img/iframe/viewer the WebView
            // loads itself) in-process from the embedded engine (#0A). POST/PATCH/DELETE
            // ride the message bridge (they carry a body, which a
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
                return try {
                    decodeAssetResponse(NativeEngine.nativeAssetRequestWithSession(path, sessionToken))
                } catch (e: Exception) {
                    android.util.Log.w(TAG, "asset serve failed for ${url.encodedPath}", e)
                    null
                }
            }

            // Emit a stable signal once the shell has rendered. Used by the CI emulator
            // smoke (REQ-AND-004) and handy for on-device diagnostics.
            override fun onPageFinished(view: WebView, url: String) {
                if (url.startsWith(APP_ORIGIN)) {
                    android.util.Log.i(TAG, "shell loaded: $url")
                }
            }
        }
        setContentView(web)

        val bridgeDecision = setupBridgeOrFail(APP_ORIGIN)
        if (!BridgeStartupPolicy.shouldStartActivityEngine(bridgeDecision)) {
            showBridgeStartupFailure(bridgeDecision)
            return
        }

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

        // Start the embedded engine off the UI thread (it touches the filesystem), then load the
        // local UI on the UI thread once it's up. The bootstrap sequence (body key + first-run wipe
        // + start) lives in EngineBootstrap so the headless DocumentsProvider (#658) shares it.
        Thread {
            // Catch Throwable: a failed System.loadLibrary (UnsatisfiedLinkError /
            // ExceptionInInitializerError) or a native panic would otherwise kill this
            // thread silently — onEngineReady would never run and the UI would hang on a
            // blank WebView with no log. Logging the start + the outcome makes engine
            // failures visible (the CI emulator smoke and on-device diagnostics rely on it).
            try {
                val token = EngineBootstrap.ensureStarted(filesDir)
                runOnUiThread { onEngineReady(if (token.isEmpty()) -1 else 1, token) }
            } catch (t: EncryptedStorageSetupException) {
                android.util.Log.e(TAG, "encrypted storage setup failed; local data was not opened", t)
                runOnUiThread { onEncryptedStorageFailed() }
            } catch (t: Throwable) {
                android.util.Log.e(TAG, "engine thread crashed starting the native engine", t)
                runOnUiThread { onEngineReady(-1, "") }
            }
        }.start()
    }

    private fun onEncryptedStorageFailed() {
        web.loadData(
            "<html><body style='font-family:sans-serif;padding:2rem'>" +
                "<h2>iSyncYou</h2><p>Encrypted storage setup failed. Local data was not opened.</p></body></html>",
            "text/html",
            "utf-8",
        )
    }

    /** Wire the session token into the WebView and load the local UI (UI thread). */
    private fun onEngineReady(readyCode: Int, token: String) {
        if (readyCode <= 0) {
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
        // The WebView loads from the stable app origin. GET assets/subresources are served
        // in-process with the trusted Activity session, and API/stream/native-control
        // traffic rides the preflighted origin-bound WebMessage bridge.
        android.util.Log.i(TAG, "engine ready (in-process), loading $APP_ORIGIN")
        pushDeviceState()
        web.loadUrl("$APP_ORIGIN/")
    }

    /**
     * Push the current device transfer conditions to the engine (#onedrive-mobile 0.9 / S-OM.9):
     * whether the active network is metered, the device is charging, and the free bytes on the
     * sync volume. The offline pass's policy gate reads these (storage floor / Wi-Fi-only /
     * charging-only). Best-effort — a read failure leaves the engine's last value (the fail-open
     * default at start), never crashing the UI.
     */
    private fun pushDeviceState() {
        try {
            val metered =
                getSystemService(ConnectivityManager::class.java)?.isActiveNetworkMetered ?: false
            val charging = getSystemService(BatteryManager::class.java)?.isCharging ?: true
            val freeBytes = StatFs(filesDir.absolutePath).availableBytes
            NativeEngine.nativeDeviceState(metered, charging, freeBytes)
        } catch (t: Throwable) {
            android.util.Log.w(TAG, "pushDeviceState failed", t)
        }
    }

    override fun onResume() {
        super.onResume()
        // Refresh the device conditions on return to the foreground (cheap; the engine's offline
        // pass reads the latest each cycle). Safe before the engine is up — it only sets a global.
        pushDeviceState()
    }

    private fun currentPushToken(): String {
        val persisted = IsyncMessagingService.currentToken(this)
        return if (persisted.isNotEmpty()) persisted else (fcmToken ?: "")
    }

    /**
     * Register the origin-bound in-process message bridge `__isyBridge` before the Activity
     * starts the engine. A missing or failed bridge is a startup failure for the WebView data
     * path, not a fallback opportunity.
     */
    private fun setupBridgeOrFail(origin: String): BridgeStartupDecision {
        val forced = BridgeStartupPolicy.forcedFailureFlag(filesDir, BuildConfig.DEBUG)
        if (forced) {
            val decision = BridgeStartupPolicy.decide(
                webMessageListenerSupported = true,
                registrationSucceeded = true,
                forcedDebugFailure = true,
            )
            android.util.Log.w(TAG, "bridge preflight failed (${decision.name})")
            return decision
        }

        val supported = WebViewFeature.isFeatureSupported(WebViewFeature.WEB_MESSAGE_LISTENER)
        if (!supported) {
            val decision = BridgeStartupPolicy.decide(
                webMessageListenerSupported = false,
                registrationSucceeded = false,
                forcedDebugFailure = false,
            )
            android.util.Log.w(TAG, "bridge preflight failed (${decision.name})")
            return decision
        }

        return try {
            WebViewCompat.addWebMessageListener(web, "__isyBridge", setOf(origin)) {
                _, message, _, _, replyProxy ->
                onBridgeMessage(message.data ?: "", replyProxy)
            }
            android.util.Log.i(TAG, "bridge listener registered for $origin")
            BridgeStartupPolicy.decide(
                webMessageListenerSupported = true,
                registrationSucceeded = true,
                forcedDebugFailure = false,
            )
        } catch (e: Exception) {
            android.util.Log.w(TAG, "bridge preflight failed (${BridgeStartupDecision.FailRegistration.name})", e)
            BridgeStartupPolicy.decide(
                webMessageListenerSupported = true,
                registrationSucceeded = false,
                forcedDebugFailure = false,
            )
        }
    }

    private fun showBridgeStartupFailure(decision: BridgeStartupDecision) {
        android.util.Log.e(TAG, "bridge startup blocked (${decision.name}); Activity engine start skipped")
        val reason = when (decision) {
            BridgeStartupDecision.FailForcedDebug -> "Bridge startup was forced to fail for verification."
            BridgeStartupDecision.FailUnsupported -> "This Android WebView does not support the required secure bridge."
            BridgeStartupDecision.FailRegistration -> "The secure bridge could not be registered."
            BridgeStartupDecision.Proceed -> "The secure bridge is available."
        }
        val html = """
            <!doctype html>
            <html>
              <head>
                <meta charset="utf-8">
                <meta name="viewport" content="width=device-width, initial-scale=1">
                <title>iSyncYou</title>
              </head>
              <body style="font-family:sans-serif;padding:2rem;line-height:1.4">
                <h2>iSyncYou</h2>
                <p>Secure WebView bridge startup failed. Local data was not opened from this screen.</p>
                <p>$reason</p>
              </body>
            </html>
        """.trimIndent()
        web.loadDataWithBaseURL(null, html, "text/html", "utf-8", null)
    }

    /** Dispatch one inbound bridge message off the UI thread (#0A). */
    private fun onBridgeMessage(data: String, reply: JavaScriptReplyProxy) {
        val validation = BridgeMessagePolicy.validateEnvelope(data)
        if (!validation.ok) {
            postBridgeError(reply, validation.id, 400, "bad_request", validation.error ?: "invalid")
            return
        }
        val obj = try {
            JSONObject(data)
        } catch (_: Exception) {
            postBridgeError(reply, validation.id, 400, "bad_request", "malformed_json")
            return
        }
        val t = validation.type ?: ""
        // One-time signal that the WebView actually routes through the bridge (not the
        // network stack) — proves the in-process data path is live. No payload.
        if (bridgeSeen.compareAndSet(false, true)) {
            android.util.Log.i(TAG, "bridge active: first message t=$t")
        }
        when (t) {
            "req" -> bridgeExecutor.execute {
                if (sessionToken.isBlank()) {
                    postBridgeError(reply, validation.id, 503, "session_not_ready", "session_not_ready")
                    return@execute
                }
                val requestJson = sanitizedBridgeRequest(obj)
                val resp = try {
                    NativeEngine.nativeBridgeRequest(requestJson)
                } catch (e: Exception) {
                    android.util.Log.w(TAG, "bridge request failed", e)
                    BridgeMessagePolicy.responseJson(
                        validation.id,
                        500,
                        JSONObject().put("error", "internal_error"),
                    )
                }
                reply.postMessage(resp)
            }
            "sub" -> {
                val jsId = validation.id
                val path = obj.optString("path")
                bridgeExecutor.execute { runBridgeStream(jsId, path, reply) }
            }
            "unsub" -> {
                bridgeStreams.remove(validation.id)?.let { NativeEngine.nativeStreamClose(it) }
            }
            // #onedrive-mobile 0.6: the WebUI asks for a biometric before a destructive op.
            // We show BiometricPrompt HERE (native, WebView-unreachable) and only on success
            // arm the server's per-action token via nativeConfirmAction. The reply carries no
            // token — just whether the human confirmed — so the WebView re-issues with `_pat`.
            "bio" -> {
                val reqId = validation.id
                val pat = obj.optString("pat")
                val label = obj.optString("label").ifEmpty { "Confirm this action" }
                runOnUiThread { runBiometric(reqId, pat, label, reply) }
            }
            "native" -> {
                handleNativeMessage(obj, validation.id, reply)
            }
        }
    }

    private fun sanitizedBridgeRequest(obj: JSONObject): String {
        val out = JSONObject()
            .put("t", "req")
            .put("id", obj.optString("id", ""))
            .put("method", obj.optString("method", "GET"))
            .put("path", obj.optString("path", "/"))
            .put("headers", BridgeMessagePolicy.sanitizeHeaders(obj.optJSONObject("headers"), sessionToken))
        if (obj.has("body") && !obj.isNull("body")) {
            out.put("body", obj.opt("body"))
        } else {
            out.put("body", JSONObject.NULL)
        }
        return out.toString()
    }

    private fun handleNativeMessage(obj: JSONObject, id: String, reply: JavaScriptReplyProxy) {
        val op = obj.optString("op")
        val payload = obj.optJSONObject("payload") ?: JSONObject()
        when (op) {
            "pushToken" -> postBridgeResponse(
                reply,
                id,
                200,
                JSONObject().put("token", currentPushToken()),
            )
            "beginNetworkGuard" -> {
                val guardId = oauthGuards.begin()
                postBridgeResponse(
                    reply,
                    id,
                    200,
                    JSONObject().put("ok", true).put("guard_id", guardId),
                )
            }
            "endNetworkGuard" -> {
                val ended = oauthGuards.end(payload.optString("guard_id", ""))
                postBridgeResponse(
                    reply,
                    id,
                    200,
                    JSONObject().put("ok", true).put("ended", ended),
                )
            }
            "openExternal" -> openExternalFromBridge(payload, id, reply)
            else -> postBridgeError(reply, id, 400, "bad_request", "unknown_op")
        }
    }

    private fun openExternalFromBridge(payload: JSONObject, id: String, reply: JavaScriptReplyProxy) {
        val kindWire = payload.optString("kind", "")
        val url = payload.optString("url", "")
        val kind = ExternalUrlPolicy.authKindFromWire(kindWire)
        if (kind == null) {
            postBridgeError(reply, id, 400, "bad_request", "missing_or_unknown_kind")
            return
        }
        val decision = ExternalUrlPolicy.classifyExternalUrl(url, kind)
        if (!decision.allowed) {
            postBridgeError(reply, id, 400, "blocked_url", decision.reason)
            return
        }
        runOnUiThread {
            try {
                startActivity(Intent(Intent.ACTION_VIEW, android.net.Uri.parse(url)))
                postBridgeResponse(reply, id, 200, JSONObject().put("ok", true))
            } catch (e: Exception) {
                android.util.Log.w(TAG, "external auth launch failed (${decision.reason})", e)
                postBridgeError(reply, id, 500, "external_launch_failed", "launch_failed")
            }
        }
    }

    private fun postBridgeError(
        reply: JavaScriptReplyProxy,
        id: String,
        status: Int,
        error: String,
        reason: String,
    ) {
        postBridgeResponse(reply, id, status, JSONObject().put("error", error).put("reason", reason))
    }

    private fun postBridgeResponse(
        reply: JavaScriptReplyProxy,
        id: String,
        status: Int,
        body: JSONObject,
    ) {
        reply.postMessage(BridgeMessagePolicy.responseJson(id, status, body))
    }

    /** Keystore alias for the biometric-bound confirmation key (#WP-8). */
    private val bioKeyAlias = "isyncyou-bio-confirm-v1"

    /**
     * Get-or-create an AndroidKeyStore AES-256-GCM key that REQUIRES a fresh strong-biometric
     * auth for every use (#WP-8), and return a `Cipher` initialised under it. Binding the
     * confirmation to this key is what makes it unforgeable: the crypto op in [runBiometric]'s
     * success callback only completes after a real biometric unlock, so a spoofed
     * `onAuthenticationSucceeded` cannot arm the token. `null` if the Keystore/key is
     * unavailable (no crypto object → the confirmation is denied, never silently bypassed).
     */
    private fun bioConfirmCipher(): Cipher? = try {
        val ks = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        val key = (ks.getEntry(bioKeyAlias, null) as? KeyStore.SecretKeyEntry)?.secretKey ?: run {
            val kg = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
            kg.init(
                KeyGenParameterSpec.Builder(
                    bioKeyAlias,
                    KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
                )
                    .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                    .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                    .setKeySize(256)
                    .setUserAuthenticationRequired(true)
                    // A new biometric enrollment invalidates the key (re-enroll = re-consent).
                    .setInvalidatedByBiometricEnrollment(true)
                    .apply {
                        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                            setUserAuthenticationParameters(0, KeyProperties.AUTH_BIOMETRIC_STRONG)
                        } else {
                            @Suppress("DEPRECATION")
                            setUserAuthenticationValidityDurationSeconds(-1)
                        }
                    }
                    .build(),
            )
            kg.generateKey()
        }
        Cipher.getInstance("AES/GCM/NoPadding").apply { init(Cipher.ENCRYPT_MODE, key) }
    } catch (e: KeyPermanentlyInvalidatedException) {
        // Biometric set changed → the old key is dead; drop it so the next attempt recreates it.
        android.util.Log.w(TAG, "bio confirm key invalidated by new enrollment; recreating", e)
        try {
            KeyStore.getInstance("AndroidKeyStore").apply { load(null) }.deleteEntry(bioKeyAlias)
        } catch (_: Exception) {
        }
        null
    } catch (e: Exception) {
        android.util.Log.w(TAG, "bio confirm key/cipher unavailable", e)
        null
    }

    /**
     * Show a `BiometricPrompt` (#onedrive-mobile 0.6, #WP-8). Prefer a **crypto-bound
     * strong-biometric** confirmation: on success, run a crypto op with the biometric-unlocked
     * Keystore key ([bioConfirmCipher]) — proof of a real strong-biometric auth. When **no strong
     * biometric is enrolled**, fall back to a **device-credential (PIN/pattern)** confirmation:
     * a CryptoObject cannot ride a device credential (Android restriction on all API levels), so
     * the fallback is a plain user-presence gate — far better than denying a legitimate destructive
     * op (requiring STRONG only regressed PIN-only devices). Either way, arm the server-side
     * per-action token over the JNI-only [NativeEngine.nativeConfirmAction] path (the WebView
     * cannot reach it) and reply `{t:"bio",id,ok}`. Must be called on the UI thread.
     */
    private fun runBiometric(reqId: String, pat: String, label: String, reply: JavaScriptReplyProxy) {
        fun done(ok: Boolean) = completeBiometric(reqId, reply, ok)
        val mgr = BiometricManager.from(this)
        val strong = BiometricManager.Authenticators.BIOMETRIC_STRONG
        // Crypto-bound path only when a strong biometric is actually enrolled; otherwise fall back
        // to a device-credential confirmation (no CryptoObject — the successful auth is the proof).
        val cipher =
            if (mgr.canAuthenticate(strong) == BiometricManager.BIOMETRIC_SUCCESS) bioConfirmCipher() else null
        val authenticators =
            if (cipher != null) strong else strong or BiometricManager.Authenticators.DEVICE_CREDENTIAL
        if (mgr.canAuthenticate(authenticators) != BiometricManager.BIOMETRIC_SUCCESS) {
            android.util.Log.w(TAG, "no biometric or device credential available; destructive op denied")
            reply.postMessage(bioReplyJson(reqId, false))
            return
        }
        lateinit var prompt: BiometricPrompt
        val timeout = Runnable {
            bioPending.remove(reqId)?.let { pending ->
                pending.prompt.cancelAuthentication()
                reply.postMessage(bioReplyJson(reqId, false))
            }
        }
        prompt = BiometricPrompt(
            this,
            ContextCompat.getMainExecutor(this),
            object : BiometricPrompt.AuthenticationCallback() {
                override fun onAuthenticationSucceeded(result: BiometricPrompt.AuthenticationResult) {
                    val armed = try {
                        if (cipher != null) {
                            // Crypto path: the op must succeed under the biometric-unlocked key —
                            // fail-closed if the result carries no crypto object.
                            result.cryptoObject?.cipher?.doFinal(pat.toByteArray(Charsets.UTF_8))
                                ?: throw IllegalStateException("no crypto object in auth result")
                        }
                        // Device-credential fallback has no crypto object; the successful auth is
                        // itself the user-presence proof.
                        NativeEngine.nativeConfirmAction(pat)
                    } catch (e: Exception) {
                        android.util.Log.w(TAG, "confirm crypto/arming failed", e)
                        false
                    }
                    done(armed)
                }

                override fun onAuthenticationError(code: Int, msg: CharSequence) = done(false)

                // A single non-match keeps the prompt up; no reply until success/error/cancel.
                override fun onAuthenticationFailed() {}
            },
        )
        bioPending.put(reqId, PendingBio(prompt, timeout))?.let { old ->
            mainHandler.removeCallbacks(old.timeout)
            old.prompt.cancelAuthentication()
        }
        mainHandler.postDelayed(timeout, BIO_TIMEOUT_MS)
        val info = BiometricPrompt.PromptInfo.Builder()
            .setTitle("Confirm action")
            .setSubtitle(label)
            .setAllowedAuthenticators(authenticators)
            .build()
        if (cipher != null) {
            prompt.authenticate(info, BiometricPrompt.CryptoObject(cipher))
        } else {
            prompt.authenticate(info)
        }
    }

    private fun completeBiometric(reqId: String, reply: JavaScriptReplyProxy, ok: Boolean) {
        val pending = bioPending.remove(reqId) ?: return
        mainHandler.removeCallbacks(pending.timeout)
        reply.postMessage(bioReplyJson(reqId, ok))
    }

    private fun bioReplyJson(reqId: String, ok: Boolean): String =
        JSONObject().put("t", "bio").put("id", reqId).put("ok", ok).toString()

    private fun cancelPendingBiometrics() {
        val pending = bioPending.values.toList()
        bioPending.clear()
        pending.forEach {
            mainHandler.removeCallbacks(it.timeout)
            it.prompt.cancelAuthentication()
        }
    }

    /** Drain one push stream, forwarding each event to the WebView until it ends (#0A). */
    private fun runBridgeStream(jsId: String, path: String, reply: JavaScriptReplyProxy) {
        if (sessionToken.isBlank()) {
            reply.postMessage(streamEndJson(jsId))
            return
        }
        val nativeId = NativeEngine.nativeStreamOpen(path, sessionToken)
        if (nativeId <= 0L) {
            reply.postMessage(streamEndJson(jsId))
            return
        }
        bridgeStreams[jsId] = nativeId
        try {
            // Keep forwarding until the stream ends (empty) or an unsub removed our mapping.
            while (bridgeStreams[jsId] == nativeId) {
                val ev = NativeEngine.nativeStreamNext(nativeId)
                if (ev.isEmpty()) break
                // ev is a JSON {event,data} object — embed it as the `ev` field.
                reply.postMessage(streamEventJson(jsId, ev))
            }
        } finally {
            bridgeStreams.remove(jsId)
            NativeEngine.nativeStreamClose(nativeId)
            reply.postMessage(streamEndJson(jsId))
        }
    }

    private fun streamEndJson(jsId: String): String =
        JSONObject().put("t", "end").put("id", jsId).toString()

    private fun streamEventJson(jsId: String, ev: String): String {
        val event = try {
            JSONObject(ev)
        } catch (_: Exception) {
            JSONObject().put("error", "bad_stream_event")
        }
        return JSONObject().put("t", "evt").put("id", jsId).put("ev", event).toString()
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
        cancelPendingBiometrics()
        bridgeStreams.values.forEach { NativeEngine.nativeStreamClose(it) }
        bridgeStreams.clear()
        oauthGuards.clear()
        OAuthGuardService.stop(this)
        bridgeExecutor.shutdownNow()
        super.onDestroy()
    }
}
