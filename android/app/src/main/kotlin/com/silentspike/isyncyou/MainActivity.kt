package com.silentspike.isyncyou

import android.Manifest
import android.app.Activity
import android.app.KeyguardManager
import android.content.Intent
import android.content.IntentFilter
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
import androidx.core.view.ViewCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.fragment.app.FragmentActivity
import androidx.lifecycle.Lifecycle
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
        const val DEVICE_CREDENTIAL_REQUEST_CODE = 640
        const val ATTACHMENT_DOCUMENT_REQUEST_CODE = 641
        val attachmentBusy = java.util.concurrent.atomic.AtomicBoolean(false)
        val providerScheduleLock = Any()

        /** The stable app origin the WebView loads from (#0A) — WebView's reserved
         *  virtual host. GET assets/subresources are served in-process via
         *  `shouldInterceptRequest`, so no mobile TCP data port is exposed. */
        const val APP_HOST = "appassets.androidplatform.net"
        const val APP_ORIGIN = "https://$APP_HOST"
    }

    private lateinit var web: WebView

    private var pendingAttachmentPath: String? = null
    private fun completeAttachmentDocument(uri: android.net.Uri?) {
        val path = pendingAttachmentPath
        pendingAttachmentPath = null
        if (uri != null && path != null) {
            val accepted = dispatchBridge {
                val success = runCatching {
                    val response = NativeEngine.nativeAssetRequestWithSession(path, sessionToken)
                    val bytes = AttachmentDownloadPolicy.body(response) ?: error("Invalid attachment response")
                    val output = requireNotNull(contentResolver.openOutputStream(uri, "w"))
                    output.use { it.write(bytes); it.flush() }
                }.isSuccess
                attachmentBusy.set(false)
                runOnUiThread {
                    android.widget.Toast.makeText(this,
                        if (success) "Attachment saved." else "Attachment could not be saved; the selected file may be incomplete.",
                        android.widget.Toast.LENGTH_LONG).show()
                }
            }
            if (!accepted) attachmentBusy.set(false)
        } else if (uri != null) {
            attachmentBusy.set(false)
            android.widget.Toast.makeText(this, "Download session expired; the selected file may be empty.",
                android.widget.Toast.LENGTH_LONG).show()
        } else attachmentBusy.set(false)
    }

    private val mainHandler = Handler(Looper.getMainLooper())

    /** The embedded engine's session token — held natively and never exposed to JS. */
    @Volatile
    private var sessionToken: String = ""

    /** Forwarding threads for the in-process bridge (#0A): one per request/stream, so a
     *  blocking `nativeStreamNext` never stalls the UI thread or another request. */
    private val bridgeExecutor = Executors.newCachedThreadPool()

    private fun dispatchBridge(block: () -> Unit): Boolean {
        val accepted = BridgeDispatch.execute(bridgeExecutor, block)
        if (!accepted) {
            android.util.Log.i(TAG, "bridge dispatch ignored after Activity teardown")
        }
        return accepted
    }

    /** JS stream id -> native stream id, for `unsub`/teardown (#0A). */
    private val bridgeStreams = ConcurrentHashMap<String, Long>()

    /** Latches true on the first bridge message so we log the live data path once (#0A). */
    private val bridgeSeen = java.util.concurrent.atomic.AtomicBoolean(false)

    private data class PendingBio(
        val requestId: String,
        val prompt: BiometricPrompt?,
        val timeout: Runnable,
        val reply: JavaScriptReplyProxy,
    )

    private data class PendingPromptDescriptor(
        val operation: String,
        val label: String,
    )

    /** One active native prompt per opaque PendingAction handle. */
    private val bioPending = BiometricPendingRegistry<PendingBio>()

    /** The system lock-screen confirmation supports only one foreground request. */
    private var activeDeviceCredential: Pair<String, String>? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        NetworkCriticalGuardRuntime.initialize(applicationContext)

        // Debuggable builds only: expose the WebView to chrome://inspect / CDP for
        // on-device debugging and verification. The FLAG_DEBUGGABLE bit is unset in release
        // builds, so this never fires there.
        if ((applicationInfo.flags and android.content.pm.ApplicationInfo.FLAG_DEBUGGABLE) != 0) {
            WebView.setWebContentsDebuggingEnabled(true)
        }
        // Own the edge-to-edge safe area once, at the native WebView boundary.
        // The embedded page therefore lays out its fixed navigation inside the
        // usable viewport on both gesture and three-button navigation devices.
        WindowCompat.setDecorFitsSystemWindows(window, false)
        @Suppress("DEPRECATION")
        window.setSoftInputMode(android.view.WindowManager.LayoutParams.SOFT_INPUT_ADJUST_RESIZE)
        val content = android.widget.FrameLayout(this)
        web = WebView(this)
        content.addView(web, android.widget.FrameLayout.LayoutParams(
            android.view.ViewGroup.LayoutParams.MATCH_PARENT,
            android.view.ViewGroup.LayoutParams.MATCH_PARENT,
        ))
        ViewCompat.setOnApplyWindowInsetsListener(content) { view, insets ->
            val bars = insets.getInsets(WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout())
            // Before API 30 adjustResize already shortens the window for IME.
            // On newer Android edge-to-edge windows receive the IME inset instead.
            val bottom = if (Build.VERSION.SDK_INT >= 30) {
                maxOf(bars.bottom, insets.getInsets(WindowInsetsCompat.Type.ime()).bottom)
            } else bars.bottom
            view.setPadding(bars.left, bars.top, bars.right, bottom)
            // The WebView is already inside the safe area; do not propagate the
            // same system/IME insets to its viewport a second time.
            WindowInsetsCompat.CONSUMED
        }
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
        expireLegacySessionCookie()
        web.webViewClient = object : WebViewClient() {
            // The app-origin UI stays in the WebView. Typed, exact authorization
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
                } catch (_: Exception) {
                    android.util.Log.w(TAG, "asset_serve_failed")
                    null
                }
            }

            // Emit a stable signal once the shell has rendered. Used by the CI emulator
            // smoke (REQ-AND-004) and handy for on-device diagnostics.
            override fun onPageFinished(view: WebView, url: String) {
                if (url.startsWith(APP_ORIGIN)) {
                    android.util.Log.i(TAG, "shell_loaded")
                }
            }
        }
        setContentView(content)
        handleProviderCallback(intent)

        val bridgeDecision = setupBridgeOrFail(APP_ORIGIN)
        if (!BridgeStartupPolicy.shouldStartActivityEngine(bridgeDecision)) {
            showBridgeStartupFailure(bridgeDecision)
            return
        }

        // FCM (#575): register the notification channel + request POST_NOTIFICATIONS
        // (Android 13+ needs a runtime grant before notifications can show).
        PushPlatform.initialize(this)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 1)
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
            } catch (_: EncryptedStorageSetupException) {
                android.util.Log.e(TAG, "encrypted_storage_setup_failed")
                runOnUiThread { onEncryptedStorageFailed() }
            } catch (_: Throwable) {
                android.util.Log.e(TAG, "engine_start_failed")
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
        NativeEngine.nativeWakeProviderRefresh()
        dispatchBridge { reconcileProviderCatchUp() }
        // The WebView loads from the stable app origin. GET assets/subresources are served
        // in-process with the trusted Activity session, and API/stream/native-control
        // traffic rides the preflighted origin-bound WebMessage bridge.
        android.util.Log.i(TAG, "engine ready (in-process), loading $APP_ORIGIN")
        pushDeviceState()
        if (MobileJobWakeupPolicy.shouldReconcileAfterEngineReady(sessionToken)) {
            dispatchBridge { MobileJobScheduler.reconcile(this) }
        }
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
        } catch (_: Throwable) {
            android.util.Log.w(TAG, "device_state_update_failed")
        }
    }

    override fun onResume() {
        super.onResume()
        // Refresh the device conditions on return to the foreground (cheap; the engine's offline
        // pass reads the latest each cycle). Safe before the engine is up — it only sets a global.
        pushDeviceState()
        try {
            if (NativeEngine.nativeNetworkDeviceHooksEnabled()) {
                NativeEngine.nativeArmCodexRefreshDeviceTestHook(filesDir.absolutePath)
            }
        } catch (_: UnsatisfiedLinkError) {
            // The default product library intentionally omits the refresh evidence hook.
        }
        if (sessionToken.isNotEmpty()) dispatchBridge { MobileJobScheduler.reconcile(this) }
        if (sessionToken.isNotEmpty()) NativeEngine.nativeWakeProviderRefresh()
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray,
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode == 1 && sessionToken.isNotEmpty()) {
            // Notification denial leaves jobs queued; a later permission callback is
            // an explicit reconciliation point, not an automatic retry loop.
            dispatchBridge { MobileJobScheduler.reconcile(this) }
        }
    }

    private fun currentPushToken(): String {
        return PushPlatform.currentToken(this)
    }

    private fun expireLegacySessionCookie() {
        try {
            android.webkit.CookieManager.getInstance().apply {
                setCookie("$APP_ORIGIN/", "isy_session=; Path=/; Max-Age=0")
                setCookie("$APP_ORIGIN/", "isy_session=; Path=/; Expires=Thu, 01 Jan 1970 00:00:00 GMT")
                flush()
            }
        } catch (_: Throwable) {
            android.util.Log.w(TAG, "legacy_session_cookie_cleanup_failed")
        }
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
        } catch (_: Exception) {
            android.util.Log.w(TAG, "bridge_registration_failed")
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
            "req" -> dispatchBridge {
                if (sessionToken.isBlank()) {
                    postBridgeError(reply, validation.id, 503, "session_not_ready", "session_not_ready")
                    return@dispatchBridge
                }
                val requestJson = sanitizedBridgeRequest(obj)
                val resp = try {
                    NativeEngine.nativeBridgeRequest(requestJson)
                } catch (_: Exception) {
                    android.util.Log.w(TAG, "bridge_request_failed")
                    BridgeMessagePolicy.responseJson(
                        validation.id,
                        500,
                        JSONObject().put("error", "internal_error"),
                    )
                }
                reconcileMobileJobsAfterSuccess(obj, resp)
                reply.postMessage(resp)
            }
            "sub" -> {
                val jsId = validation.id
                val path = obj.optString("path")
                val capabilityToken = obj.optString("cap_token", "")
                dispatchBridge { runBridgeStream(jsId, path, capabilityToken, reply) }
            }
            "unsub" -> {
                bridgeStreams.remove(validation.id)?.let { NativeEngine.nativeStreamClose(it) }
            }
            // #onedrive-mobile 0.6: the WebUI asks for a biometric before a destructive op.
            // We show BiometricPrompt HERE (native, WebView-unreachable) and only on success
            // arm the server's per-action token via nativeConfirmAction. The reply carries no
            // token — just whether the human confirmed — so the WebView re-issues with the
            // public pending handle in X-Per-Action-Token.
            "bio" -> {
                val reqId = validation.id
                val pat = obj.optString("pat")
                runOnUiThread { runBiometric(reqId, pat, reply) }
            }
            "native" -> {
                handleNativeMessage(obj, validation.id, reply)
            }
        }
    }

    private fun reconcileMobileJobsAfterSuccess(request: JSONObject, response: String) {
        val method = request.optString("method", "")
        val path = request.optString("path", "")
        val status = runCatching { JSONObject(response).optInt("status", -1) }.getOrDefault(-1)
        if (MobileJobWakeupPolicy.shouldReconcile(method, path, status)) {
            MobileJobScheduler.reconcile(this)
        }
    }

    private fun sanitizedBridgeRequest(obj: JSONObject): String {
        val path = obj.optString("path", "/")
        val storageNotLow = if (BridgeMessagePolicy.requiresTrustedStorageNotLow(path)) {
            registerReceiver(null, IntentFilter(Intent.ACTION_DEVICE_STORAGE_LOW)) == null
        } else {
            null
        }
        val out = JSONObject()
            .put("t", "req")
            .put("id", obj.optString("id", ""))
            .put("method", obj.optString("method", "GET"))
            .put("path", path)
            .put(
                "headers",
                BridgeMessagePolicy.sanitizeHeaders(
                    obj.optJSONObject("headers"),
                    sessionToken,
                    storageNotLow,
                ),
            )
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
            "bridgeCapabilities" -> postBridgeResponse(reply, id, 200, JSONObject().put("scoped_attachment_download", true))
            "downloadProviderAttachment" -> {
                if (sessionToken.isBlank()) {
                    postBridgeError(reply, id, 503, "unavailable", "session_not_ready")
                } else runOnUiThread {
                    if (!attachmentBusy.compareAndSet(false, true)) {
                        postBridgeError(reply, id, 409, "unavailable", "download_pending")
                    } else {
                        val query = android.net.Uri.Builder().appendQueryParameter("account", payload.getString("account"))
                            .appendQueryParameter("service", "mail").appendQueryParameter("id", payload.getString("id"))
                            .appendQueryParameter("blob_id", payload.getString("blob_id"))
                            .apply { if (payload.has("mail_scope")) appendQueryParameter("mail_scope", payload.getString("mail_scope")) }
                            .build().encodedQuery
                        pendingAttachmentPath = "/api/v1/attachment?$query"
                        try {
                            val intent = Intent(Intent.ACTION_CREATE_DOCUMENT).apply {
                                addCategory(Intent.CATEGORY_OPENABLE)
                                type = "application/octet-stream"
                                putExtra(Intent.EXTRA_TITLE, AttachmentDownloadPolicy.filename(payload.getString("filename")))
                            }
                            startActivityForResult(intent, ATTACHMENT_DOCUMENT_REQUEST_CODE)
                            postBridgeResponse(reply, id, 200, JSONObject().put("opened", true))
                        } catch (_: Exception) {
                            pendingAttachmentPath = null
                            attachmentBusy.set(false)
                            postBridgeError(reply, id, 503, "unavailable", "document_picker_unavailable")
                        }
                    }
                }
            }
            "providerCreateAccount" -> dispatchBridge {
                val success = NativeEngine.nativeProviderCreateAccount(filesDir.absolutePath, payload.toString()) == 1
                if (success) postBridgeResponse(reply, id, 200, JSONObject().put("created", true))
                else postBridgeError(reply, id, 400, "unavailable", "provider_account_setup_failed")
            }
            "providerQuarantineList" -> dispatchBridge {
                val result = runCatching { JSONObject(NativeEngine.nativeProviderQuarantineList(filesDir.absolutePath, payload.getString("account"))) }.getOrNull()
                if (result?.optBoolean("ok") == true) postBridgeResponse(reply, id, 200, result)
                else postBridgeError(reply, id, 503, "unavailable", "grant_recovery_unavailable")
            }
            "providerQuarantineRevoke" -> dispatchBridge {
                val result = NativeEngine.nativeProviderQuarantineRevoke(filesDir.absolutePath,
                    payload.getString("account"), payload.getString("recovery_id"))
                if (result == 1) postBridgeResponse(reply, id, 200, JSONObject().put("revoked", true))
                else postBridgeError(reply, id, 503, "unavailable", "grant_revocation_unconfirmed")
            }
            "providerSignIn" -> providerSignIn(payload.optString("account"), id, reply)
            "providerAuthResolve" -> resolveUnknownProviderAuth(payload.optString("account"), id, reply)
            "providerAuthCancelPending" -> dispatchBridge {
                val cancelled = NativeEngine.nativeProviderCancelPending(filesDir.absolutePath, payload.getString("account")) == 1
                if (cancelled) postBridgeResponse(reply, id, 200, JSONObject().put("cancelled", true))
                else postBridgeError(reply, id, 409, "unavailable", "provider_pending_changed")
            }
            "providerPushSetup" -> providerPushSetup(payload.optString("account"), id, reply)
            "providerPushCancelPrepare" -> cancelUncertainPushPrepare(payload.getString("account"), id, reply)
            "providerPushDetachCancelled" -> detachCancelledPush(payload.getString("account"), id, reply)
            "providerPushRegisterReplacement" -> registerReplacementPush(payload.getString("account"), id, reply)
            "providerPushStatus" -> dispatchBridge {
                val account = payload.optString("account")
                val result = runCatching {
                    val identity = PushInstallationKey.getOrCreate(this, account)
                    val state = PushEnrollmentStore.get(this, identity.installation)
                    val stage = state?.optString("stage", "not_registered") ?: "not_registered"
                    val expiry = state?.optString("expires", "")?.let(PushJmap::expiryMillis)
                    val origin = NativeEngine.nativeProviderOrigin(filesDir.absolutePath, account)
                    val status = when {
                        state != null && (origin.isEmpty() || state.optString("origin") != origin) -> "account_changed"
                        stage == "credentials_ready" && PushCancelPolicy.canClaim(stage, state?.optLong("expires", 0) ?: 0) -> "credentials_expired"
                        PushStatusPolicy.retainStage(stage) -> stage
                        state?.optBoolean("degraded", false) == true -> "degraded"
                        stage == "active" && expiry == null -> "expiry_unknown"
                        stage == "active" && expiry != null && expiry <= System.currentTimeMillis() -> "expired"
                        else -> stage
                    }
                    JSONObject().put("status", status).put("catch_up_pending", state?.optBoolean("hint_pending", false) ?: false)
                        .put("last_hint_at", state?.optLong("last_hint_at", 0) ?: 0)
                        .put("last_sync_at", state?.optLong("last_sync_at", 0) ?: 0)
                }.getOrElse { JSONObject().put("status", "unavailable") }
                postBridgeResponse(reply, id, 200, result)
            }
            "providerStatus", "providerSignOut" -> dispatchBridge {
                val account = payload.optString("account")
                val result = if (op == "providerStatus") {
                    NativeEngine.nativeProviderStatus(filesDir.absolutePath, account)
                } else {
                    synchronized(providerScheduleLock) {
                        val journalStatus = NativeEngine.nativeProviderAuthJournalStatus(filesDir.absolutePath)
                        val signedOut = if (journalStatus in 0..1 && revokePushBeforeLogout(account))
                            NativeEngine.nativeProviderLogout(filesDir.absolutePath, account) else 0
                        if (signedOut == 1) reconcileProviderCatchUp()
                        signedOut
                    }
                }
                if (result < 0 || (op == "providerSignOut" && result != 1)) {
                    postBridgeError(reply, id, 503, "unavailable", "provider_operation_failed")
                } else {
                    val body = JSONObject().put("connected", op == "providerStatus" && result == 1)
                    if (op == "providerStatus") {
                        val authState = when (NativeEngine.nativeProviderAuthJournalAccountStatus(filesDir.absolutePath, account)) {
                            0 -> "idle"
                            1 -> "pending"
                            2 -> "outcome_unknown"
                            else -> "unavailable"
                        }
                        body.put("auth_state", authState)
                    }
                    postBridgeResponse(reply, id, 200, body)
                }
            }
            "pushToken" -> postBridgeResponse(
                reply,
                id,
                200,
                JSONObject().put("token", currentPushToken()),
            )
            "beginNetworkGuard" -> {
                dispatchBridge {
                    val reason = NetworkGuardReason.fromWire(payload.optString("reason", ""))
                    val result = if (reason == null) {
                        NetworkGuardBeginResult(null, "invalid_reason")
                    } else {
                        NetworkCriticalGuardRuntime.begin(reason)
                    }
                    if (result.ok) {
                        postBridgeResponse(
                            reply,
                            id,
                            200,
                            JSONObject().put("ok", true).put("guard_id", result.guardId),
                        )
                    } else {
                        android.util.Log.w(TAG, "network guard unavailable (${result.error ?: "start_failed"})")
                        postBridgeError(reply, id, 503, "unavailable", "network_guard_unavailable")
                    }
                }
            }
            "endNetworkGuard" -> {
                val ended = NetworkCriticalGuardRuntime.end(payload.optString("guard_id", ""))
                postBridgeResponse(
                    reply,
                    id,
                    200,
                    JSONObject().put("ok", true).put("ended", ended),
                )
            }
            "bindNetworkGuard" -> {
                val bound = NetworkCriticalGuardRuntime.bindTurn(
                    payload.optString("guard_id", ""),
                    payload.optString("turn", ""),
                )
                postBridgeResponse(reply, id, if (bound) 200 else 409, JSONObject().put("ok", bound))
            }
            "captureNetworkSnapshot" -> {
                dispatchBridge {
                    val guardId = payload.optString("guard_id", "")
                    val lease = NetworkCriticalGuardRuntime.activeLease(guardId)
                    if (lease == null) {
                        postBridgeError(reply, id, 409, "unavailable", "network_guard_unavailable")
                        return@dispatchBridge
                    }
                    val hook = NetworkDeviceTestHook.take(applicationContext)
                    if (hook == NetworkDeviceTestHook.ForegroundGuardUnavailable) {
                        postBridgeError(reply, id, 503, "unavailable", "network_guard_unavailable")
                        return@dispatchBridge
                    }
                    val captured = NetworkSnapshotProvider.capture(applicationContext)
                    val snapshot = if (hook == NetworkDeviceTestHook.NoValidatedNetwork) {
                        captured.copy(validatedCapability = false)
                    } else {
                        captured
                    }
                    val transportHook = when (hook) {
                        NetworkDeviceTestHook.ConnectTimeout,
                        NetworkDeviceTestHook.TlsFailed,
                        NetworkDeviceTestHook.HttpFailed,
                        -> hook.wire
                        else -> ""
                    }
                    val handle = NativeEngine.nativeRegisterNetworkSnapshot(
                        guardId,
                        lease.reason.wire,
                        snapshot.activeNetwork,
                        snapshot.internetCapability,
                        snapshot.validatedCapability,
                        snapshot.metered,
                        snapshot.restrictBackground,
                        snapshot.notificationsVisible,
                        transportHook,
                    )
                    if (handle.isBlank()) {
                        postBridgeError(reply, id, 503, "unavailable", "network_snapshot_unavailable")
                    } else {
                        postBridgeResponse(reply, id, 200, JSONObject().put("snapshot_id", handle))
                    }
                }
            }
            "openNetworkSettings" -> {
                val hint = NetworkSettingsHint.fromWire(payload.optString("hint", ""))
                if (hint == null) {
                    postBridgeError(reply, id, 400, "bad_request", "missing_or_unknown_settings_hint")
                    return
                }
                runOnUiThread {
                    val opened = NetworkSettingsPolicy.open(this, hint)
                    if (opened) {
                        postBridgeResponse(reply, id, 200, JSONObject().put("ok", true))
                    } else {
                        postBridgeError(reply, id, 503, "unavailable", "settings_unavailable")
                    }
                }
            }
            "openExternal" -> openExternalFromBridge(payload, id, reply)
            else -> postBridgeError(reply, id, 400, "bad_request", "unknown_op")
        }
    }

    private fun reconcileProviderCatchUp() = synchronized(providerScheduleLock) {
        val response = runCatching {
            JSONObject(NativeEngine.nativeConnectedProviderAccounts(filesDir.absolutePath))
        }.getOrNull()
        if (response?.optBoolean("ok") != true) {
            // Preserve a retry when encrypted account state is temporarily unavailable.
            ProviderCatchUpWorker.schedule(this)
            return@synchronized
        }
        if (response.optJSONArray("accounts")?.length() == 0) ProviderCatchUpWorker.cancel(this)
        else ProviderCatchUpWorker.schedule(this)
    }

    private fun resolveUnknownProviderAuth(account: String, id: String, reply: JavaScriptReplyProxy) {
        dispatchBridge {
            val connected = NativeEngine.nativeProviderAuthRecoveryPrepare(filesDir.absolutePath, account)
            if (connected < 0) {
                postBridgeError(reply, id, 409, "unavailable", "provider_auth_recovery_changed")
                return@dispatchBridge
            }
            runOnUiThread {
                val local = if (connected == 1) "A locally stored sign-in is still present; its server validity has not been checked." else "No confirmed local sign-in is available, or revocation is pending."
                android.app.AlertDialog.Builder(this)
                    .setTitle("Uncertain sign-in")
                    .setMessage("Account: $account\n\nThe server may have issued an authorization even though the result was lost. $local Clearing this local waiting state will not revoke an unknown server grant. Check and revoke any unexpected grant in the provider's account settings. Continue only if you accept that limitation.")
                    .setNegativeButton("Keep waiting") { _, _ ->
                        NativeEngine.nativeProviderAuthRecoveryCancel()
                        postBridgeResponse(reply, id, 200, JSONObject().put("status", "unchanged"))
                    }
                    .setOnCancelListener {
                        NativeEngine.nativeProviderAuthRecoveryCancel()
                        postBridgeResponse(reply, id, 200, JSONObject().put("status", "unchanged"))
                    }
                    .setPositiveButton("Clear waiting state") { _, _ ->
                        dispatchBridge {
                            val cleared = synchronized(providerScheduleLock) {
                                NativeEngine.nativeProviderAuthRecoveryAbort(filesDir.absolutePath, account) == 1
                            }
                            if (cleared) postBridgeResponse(reply, id, 200, JSONObject().put("status", "cleared"))
                            else postBridgeError(reply, id, 409, "unavailable", "provider_auth_recovery_changed")
                        }
                    }.show()
            }
        }
    }

    private fun providerSignIn(account: String, id: String, reply: JavaScriptReplyProxy) {
        dispatchBridge {
            if (NativeEngine.nativeProviderAuthJournalStatus(filesDir.absolutePath) == 2) {
                postBridgeError(reply, id, 409, "unavailable", "provider_auth_outcome_unknown")
                return@dispatchBridge
            }
            val redirect = "${packageName}:/oauth/callback"
            val url = NativeEngine.nativeProviderBegin(filesDir.absolutePath, account, redirect)
            if (url.isEmpty()) {
                postBridgeError(reply, id, 503, "unavailable", "provider_authorization_unavailable")
                return@dispatchBridge
            }
            // The URL comes exclusively from native discovery and PKCE, not from WebView input.
            val parsed = runCatching { java.net.URI(url) }.getOrNull()
            if (parsed?.scheme != "https" || parsed.host.isNullOrBlank() || parsed.rawUserInfo != null) {
                NativeEngine.nativeProviderCancel()
                postBridgeError(reply, id, 503, "unavailable", "invalid_provider_authorization")
                return@dispatchBridge
            }
            runOnUiThread {
                try {
                    startActivity(Intent(Intent.ACTION_VIEW, android.net.Uri.parse(url)))
                    postBridgeResponse(reply, id, 200, JSONObject().put("opened", true))
                } catch (_: Exception) {
                    NativeEngine.nativeProviderCancel()
                    postBridgeError(reply, id, 503, "unavailable", "browser_unavailable")
                }
            }
        }
    }

    private fun detachCancelledPush(account: String, id: String, reply: JavaScriptReplyProxy) {
        dispatchBridge {
            val identity = runCatching { PushInstallationKey.existing(this, account) }.getOrNull()
            val state = identity?.let { runCatching { PushEnrollmentStore.get(this, it.installation) }.getOrNull() }
            val origin = NativeEngine.nativeProviderOrigin(filesDir.absolutePath, account)
            if (identity == null || state == null || origin.isEmpty() || state.optString("account") != account ||
                state.optString("origin") != origin || !PushCancelPolicy.canDetach(state.optString("stage"))) {
                postBridgeError(reply, id, 409, "unavailable", "push_reconcile_changed"); return@dispatchBridge
            }
            val endpoint = state.optString("endpoint")
            val claimed = PushEnrollmentStore.update(this, identity.installation) {
                if (it.optString("account") != account || it.optString("origin") != origin ||
                    it.optString("endpoint") != endpoint || !PushCancelPolicy.canDetach(it.optString("stage"))) return@update false
                it.put("stage", "distributor_unregister_unknown"); true
            }
            if (!claimed) { postBridgeError(reply, id, 409, "unavailable", "push_reconcile_changed"); return@dispatchBridge }
            try {
                // Connector removes only this local registration and sends the
                // old token's unregister broadcast; it does not acknowledge it.
                org.unifiedpush.android.connector.UnifiedPush.unregister(this, identity.installation, WebPushKeyManager(this))
                val saved = PushEnrollmentStore.update(this, identity.installation) {
                    if (it.optString("account") != account || it.optString("origin") != origin ||
                        it.optString("endpoint") != endpoint || it.optString("stage") != "distributor_unregister_unknown") return@update false
                    it.put("stage", "distributor_detached"); true
                }
                check(saved)
                postBridgeResponse(reply, id, 200, JSONObject().put("status", "detached"))
            } catch (_: Exception) { postBridgeError(reply, id, 503, "unavailable", "push_detach_unconfirmed") }
        }
    }

    private fun registerReplacementPush(account: String, id: String, reply: JavaScriptReplyProxy) {
        dispatchBridge {
            val identity = runCatching { PushInstallationKey.existing(this, account) }.getOrNull()
            val state = identity?.let { runCatching { PushEnrollmentStore.get(this, it.installation) }.getOrNull() }
            val origin = NativeEngine.nativeProviderOrigin(filesDir.absolutePath, account)
            if (identity == null || state == null || origin.isEmpty() || state.optString("account") != account ||
                state.optString("origin") != origin || !PushCancelPolicy.canRegisterReplacement(state.optString("stage"))) {
                postBridgeError(reply, id, 409, "unavailable", "push_reconcile_changed"); return@dispatchBridge
            }
            val endpoint = state.optString("endpoint")
            val vapid = if (state.isNull("vapid")) null else state.optString("vapid")
            if (vapid != null && !vapid.matches(Regex("[A-Za-z0-9_-]{87}"))) {
                postBridgeError(reply, id, 503, "unavailable", "push_provider_unavailable"); return@dispatchBridge
            }
            runOnUiThread {
                org.unifiedpush.android.connector.UnifiedPush.tryUseCurrentOrDefaultDistributor(this) { selected ->
                    if (!selected) { postBridgeError(reply, id, 503, "unavailable", "push_distributor_unavailable"); return@tryUseCurrentOrDefaultDistributor }
                    try {
                        val claimed = PushEnrollmentStore.update(this, identity.installation) {
                            if (it.optString("account") != account || it.optString("origin") != origin ||
                                it.optString("endpoint") != endpoint || !PushCancelPolicy.canRegisterReplacement(it.optString("stage"))) return@update false
                            it.remove("endpoint"); it.remove("public_key"); it.remove("auth"); it.remove("degraded")
                            it.put("stage", "registering"); true
                        }
                        check(claimed)
                        org.unifiedpush.android.connector.UnifiedPush.register(this, identity.installation, "iSyncYou", vapid, WebPushKeyManager(this))
                        postBridgeResponse(reply, id, 200, JSONObject().put("status", "registering"))
                    } catch (_: Exception) { postBridgeError(reply, id, 503, "unavailable", "push_registration_unconfirmed") }
                }
            }
        }
    }

    private fun cancelUncertainPushPrepare(account: String, id: String, reply: JavaScriptReplyProxy) {
        dispatchBridge {
            val identity = runCatching { PushInstallationKey.existing(this, account) }.getOrNull()
            val state = identity?.let { runCatching { PushEnrollmentStore.get(this, it.installation) }.getOrNull() }
            val origin = NativeEngine.nativeProviderOrigin(filesDir.absolutePath, account)
            if (identity == null || state == null || origin.isEmpty() || state.optString("account") != account ||
                state.optString("origin") != origin || !PushCancelPolicy.canClaim(state.optString("stage"), state.optLong("expires", 0))) {
                postBridgeError(reply, id, 409, "unavailable", "push_reconcile_changed")
                return@dispatchBridge
            }
            val endpoint = state.optString("endpoint")
            try {
                val result = PushBroker.operation(this, account, endpoint, "cancel", beforeDispatch = {
                    if (PushInstallationKey.existing(this, account) != identity) return@operation false
                    PushEnrollmentStore.update(this, identity.installation) {
                        if (it.optString("account") != account || it.optString("origin") != origin ||
                            it.optString("endpoint") != endpoint || !PushCancelPolicy.canClaim(it.optString("stage"), it.optLong("expires", 0))) return@update false
                        it.put("stage", "prepare_cancel_unknown"); true
                    }
                })
                check(result.optString("status") == "cancelled")
                val binding = result.optString("binding")
                val nextStage = checkNotNull(PushCancelPolicy.confirmedStage(binding))
                check(PushInstallationKey.existing(this, account) == identity)
                val saved = PushEnrollmentStore.update(this, identity.installation) {
                    if (it.optString("account") != account || it.optString("origin") != origin ||
                        it.optString("endpoint") != endpoint || it.optString("stage") != "prepare_cancel_unknown") return@update false
                    if (binding == "absent") {
                        it.remove("username"); it.remove("password"); it.remove("expires")
                    }
                    it.put("stage", nextStage)
                    true
                }
                if (!saved) throw IllegalStateException("Push reconciliation changed")
                postBridgeResponse(reply, id, 200, JSONObject().put("status", if (binding == "absent") "cancelled" else "binding_present"))
            } catch (_: Exception) {
                postBridgeError(reply, id, 503, "unavailable", "push_reconcile_unconfirmed")
            }
        }
    }

    private fun providerPushSetup(account: String, id: String, reply: JavaScriptReplyProxy) {
        dispatchBridge {
            val identity = runCatching { PushInstallationKey.getOrCreate(this, account) }.getOrNull()
            if (identity == null || NativeEngine.nativeProviderStatus(filesDir.absolutePath, account) != 1) {
                postBridgeError(reply, id, 503, "unavailable", "provider_login_required")
                return@dispatchBridge
            }
            val configured = NativeEngine.nativeProviderBrokerConfigured(filesDir.absolutePath, account)
            if (configured != 1) {
                if (configured < 0) {
                    postBridgeError(reply, id, 503, "unavailable", "provider_configuration_unavailable")
                    return@dispatchBridge
                }
                runOnUiThread {
                    val field = android.widget.EditText(this).apply {
                        hint = "https://your-server.example/api/push/"
                        inputType = android.text.InputType.TYPE_CLASS_TEXT or android.text.InputType.TYPE_TEXT_VARIATION_URI
                    }
                    android.app.AlertDialog.Builder(this).setTitle("Push service address")
                        .setMessage("Enter the push broker address on the same server as this account.")
                        .setView(field).setNegativeButton("Cancel", null)
                        .setPositiveButton("Save") { _, _ ->
                            val address = field.text.toString().trim()
                            dispatchBridge {
                                val saved = NativeEngine.nativeProviderSetBroker(filesDir.absolutePath, account, address) == 1
                                runOnUiThread {
                                    android.widget.Toast.makeText(this,
                                        if (saved) "Push address saved. Start setup again." else "Push address is invalid or unavailable.",
                                        android.widget.Toast.LENGTH_LONG).show()
                                }
                            }
                        }.show()
                    postBridgeResponse(reply, id, 200, JSONObject().put("status", "configuration_required"))
                }
                return@dispatchBridge
            }
            val state = runCatching { PushEnrollmentStore.get(this, identity.installation) }.getOrNull()
            val origin = NativeEngine.nativeProviderOrigin(filesDir.absolutePath, account)
            if (origin.isEmpty() || (state != null && state.optString("origin") != origin)) {
                postBridgeError(reply, id, 409, "unavailable", "provider_account_changed")
                return@dispatchBridge
            }
            if (state?.optString("stage") == "credentials_ready") {
                if (state.optLong("expires", 0) <= System.currentTimeMillis() / 1000) {
                    postBridgeError(reply, id, 503, "unavailable", "push_enrollment_expired")
                    return@dispatchBridge
                }
                // Credentials are displayed by native UI only and never enter the WebView.
                val pushOrigin = runCatching {
                    val endpoint = java.net.URI(state.getString("endpoint"))
                    require(endpoint.scheme == "https" && !endpoint.host.isNullOrBlank() && endpoint.rawUserInfo == null)
                    "https://${endpoint.host}" + if (endpoint.port == -1) "" else ":${endpoint.port}"
                }.getOrNull()
                if (pushOrigin == null) {
                    postBridgeError(reply, id, 503, "unavailable", "push_endpoint_invalid")
                    return@dispatchBridge
                }
                runOnUiThread {
                    android.app.AlertDialog.Builder(this)
                        .setTitle("Configure push distributor")
                        .setMessage("Enter these temporary credentials for $pushOrigin in your push distributor:\n\nUser: ${state.optString("username")}\nPassword: ${state.optString("password")}\n\nAfter configuration, return here and continue.")
                        .setNegativeButton("Later", null)
                        .setPositiveButton("Configured") { _, _ ->
                            dispatchBridge {
                                try {
                                    state.put("stage", "awaiting_proof")
                                    PushEnrollmentStore.put(this, identity.installation, state)
                                    PushBroker.operation(this, account, state.getString("endpoint"), "deliver")
                                } catch (_: Exception) {
                                    val latest = PushEnrollmentStore.get(this, identity.installation)
                                    if (latest?.optString("stage") == "awaiting_proof") {
                                        PushEnrollmentStore.put(this, identity.installation, latest.put("stage", "delivery_unknown"))
                                    }
                                }
                            }
                        }.show()
                    postBridgeResponse(reply, id, 200, JSONObject().put("status", "credentials_ready"))
                }
                return@dispatchBridge
            }
            if (state != null) {
                if (state.optString("stage") in setOf("create_unknown", "destroy_unknown", "replacement_destroy_unknown", "renew_unknown")) {
                    PushScheduler.enqueue(this, identity.installation)
                }
                postBridgeResponse(reply, id, 200, JSONObject().put("status", state.optString("stage", "unavailable")))
                return@dispatchBridge
            }
            val metadata = runCatching {
                val response = JSONObject(NativeEngine.nativeProviderPush(filesDir.absolutePath, account, "metadata", "{}"))
                check(response.optBoolean("ok"))
                response.getJSONObject("data")
            }.getOrNull()
            if (metadata == null) {
                postBridgeError(reply, id, 503, "unavailable", "push_provider_unavailable")
                return@dispatchBridge
            }
            val vapid = if (metadata.isNull("vapid")) null else metadata.optString("vapid")
            if (vapid != null && !vapid.matches(Regex("[A-Za-z0-9_-]{87}"))) {
                postBridgeError(reply, id, 503, "unavailable", "push_provider_unavailable")
                return@dispatchBridge
            }
            runOnUiThread {
                org.unifiedpush.android.connector.UnifiedPush.tryUseCurrentOrDefaultDistributor(this) { selected ->
                    if (!selected) {
                        postBridgeError(reply, id, 503, "unavailable", "push_distributor_unavailable")
                    } else {
                        try {
                            PushEnrollmentStore.put(this, identity.installation,
                                JSONObject().put("account", account).put("origin", origin).put("stage", "registering")
                                    .put("vapid", vapid ?: JSONObject.NULL).put("session_state", metadata.getString("session_state")))
                            org.unifiedpush.android.connector.UnifiedPush.register(this, identity.installation,
                                "iSyncYou", vapid, WebPushKeyManager(this))
                            PushScheduler.scheduleCatchUp(this, identity.installation)
                            postBridgeResponse(reply, id, 200, JSONObject().put("status", "registering"))
                        } catch (_: Exception) {
                            postBridgeError(reply, id, 503, "unavailable", "push_registration_failed")
                        }
                    }
                }
            }
        }
    }

    private fun revokePushBeforeLogout(account: String): Boolean = runCatching {
        val identity = PushInstallationKey.existing(this, account) ?: return true
        val state = PushEnrollmentStore.get(this, identity.installation)
        if (state != null) {
            val origin = NativeEngine.nativeProviderOrigin(filesDir.absolutePath, account)
            check(origin.isNotEmpty() && state.optString("origin") == origin)
            val initialStage = state.optString("stage")
            if (initialStage in setOf("active", "verification_pending")) {
                val id = state.getString("subscription_id")
                check(PushEnrollmentStore.update(this, identity.installation) {
                    if (it.optString("stage") != initialStage || it.optString("subscription_id") != id) return@update false
                    it.put("stage", "destroy_unknown"); true
                })
                val destroyed = PushJmap.call(this, account, "destroy", JSONObject().put("id", id))
                check(destroyed.optString("status") == "applied")
                check(PushEnrollmentStore.update(this, identity.installation) {
                    if (it.optString("stage") != "destroy_unknown") return@update false
                    it.put("stage", "broker_bound_logout"); true
                })
            }
            // Unknown or provisional server mutations need reconciliation, never blind logout.
            check(PushEnrollmentStore.update(this, identity.installation) {
                if (it.optString("stage") !in setOf("broker_bound", "broker_bound_logout") ||
                    it.optString("endpoint") != state.optString("endpoint")) return@update false
                it.put("stage", "revoke_unknown"); true
            })
            val result = PushBroker.operation(this, account, state.getString("endpoint"), "revoke")
            check(result.optString("status") == "revoked")
            check(PushEnrollmentStore.removeIf(this, identity.installation) {
                it.optString("stage") == "revoke_unknown" && it.optString("endpoint") == state.optString("endpoint")
            })
            org.unifiedpush.android.connector.UnifiedPush.unregister(this, identity.installation, WebPushKeyManager(this))
            PushScheduler.cancel(this, identity.installation)
            state.optString("expires", "").takeIf { it.isNotEmpty() }?.let {
                androidx.work.WorkManager.getInstance(this).cancelUniqueWork("provider-push-renew:${identity.installation}:$it")
            }
        }
        PushInstallationKey.deleteAfterRevoke(this, account)
        true
    }.getOrDefault(false)

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        handleProviderCallback(intent)
    }

    private fun handleProviderCallback(intent: Intent?) {
        if (intent?.action != Intent.ACTION_VIEW) return
        val uri = intent.data ?: return
        if (uri.scheme != packageName || uri.host != null || uri.path != "/oauth/callback" ||
            uri.fragment != null || uri.toString().length > 8192) return
        // Remove the sensitive URI from the Activity intent before any asynchronous work.
        intent.data = null
        dispatchBridge {
            val started = runCatching { EngineBootstrap.ensureStarted(filesDir).isNotEmpty() }.getOrDefault(false)
            val outcome = synchronized(providerScheduleLock) {
                val result = if (started) NativeEngine.nativeProviderFinish(filesDir.absolutePath, uri.toString()) else 0
                if (result == 1) {
                    NativeEngine.nativeWakeProviderRefresh()
                    ProviderCatchUpWorker.schedule(this)
                }
                result
            }
            val journalUnknown = outcome == 0 && NativeEngine.nativeProviderAuthJournalStatus(filesDir.absolutePath) == 2
            runOnUiThread {
                if (::web.isInitialized) {
                    val result = when (outcome) {
                        1 -> "connected"
                        2 -> "revocation_pending"
                        3 -> "revocation_unrecoverable"
                        else -> if (journalUnknown) "outcome_unknown" else "failed"
                    }
                    web.evaluateJavascript("window.dispatchEvent(new CustomEvent('isy-provider-auth', {detail:'$result'}))", null)
                }
            }
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
            } catch (_: Exception) {
                android.util.Log.w(TAG, "external_auth_launch_failed")
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
    } catch (_: KeyPermanentlyInvalidatedException) {
        // Biometric set changed → the old key is dead; drop it so the next attempt recreates it.
        android.util.Log.w(TAG, "biometric_key_invalidated")
        try {
            KeyStore.getInstance("AndroidKeyStore").apply { load(null) }.deleteEntry(bioKeyAlias)
        } catch (_: Exception) {
        }
        null
    } catch (_: Exception) {
        android.util.Log.w(TAG, "biometric_key_unavailable")
        null
    }

    /** Show a native prompt using only the Rust-owned pending descriptor. */
    private fun runBiometric(reqId: String, pat: String, reply: JavaScriptReplyProxy) {
        val descriptor = try {
            JSONObject(NativeEngine.nativeDescribePendingAction(pat)).let { result ->
                if (result.optString("status") != "ok") {
                    return reply.postMessage(bioReplyJson(reqId, false, "descriptor_unavailable"))
                }
                val op = result.optString("op")
                val service = result.optString("service")
                if (op.isBlank() || service.isBlank()) {
                    return reply.postMessage(bioReplyJson(reqId, false, "descriptor_unavailable"))
                }
                val label = BiometricLabelPolicy.label(op, service)
                    ?: return reply.postMessage(bioReplyJson(reqId, false, "descriptor_unavailable"))
                PendingPromptDescriptor(op, label)
            }
        } catch (_: Exception) {
            android.util.Log.w(TAG, "pending_action_descriptor_unavailable")
            reply.postMessage(bioReplyJson(reqId, false, "descriptor_unavailable"))
            return
        }
        if (!lifecycle.currentState.isAtLeast(Lifecycle.State.RESUMED)) {
            reply.postMessage(bioReplyJson(reqId, false, "not_foreground"))
            return
        }
        val mgr = BiometricManager.from(this)
        val strong = BiometricManager.Authenticators.BIOMETRIC_STRONG
        val strongAvailable = mgr.canAuthenticate(strong) == BiometricManager.BIOMETRIC_SUCCESS
        val cipher = if (strongAvailable) bioConfirmCipher() else null
        val credential = BiometricManager.Authenticators.DEVICE_CREDENTIAL
        val credentialAvailable = BiometricPolicy.credentialAvailable(
            apiLevel = Build.VERSION.SDK_INT,
            biometricManagerAvailable = Build.VERSION.SDK_INT >= Build.VERSION_CODES.R &&
                mgr.canAuthenticate(credential) == BiometricManager.BIOMETRIC_SUCCESS,
            keyguardDeviceSecure = getSystemService(KeyguardManager::class.java)?.isDeviceSecure == true,
        )
        val decision = BiometricPolicy.chooseForOperation(
            descriptor.operation,
            strongAvailable,
            cipher != null,
            credentialAvailable,
        )
        if (decision == null) {
            android.util.Log.w(TAG, "biometric confirmation unavailable or strong cipher failed")
            reply.postMessage(bioReplyJson(reqId, false, "not_available"))
            return
        }
        if (decision.mode == BiometricMode.DeviceCredential) {
            runDeviceCredential(reqId, pat, descriptor.label, reply)
            return
        }
        lateinit var prompt: BiometricPrompt
        val timeout = Runnable {
            takePendingBiometric(pat, reqId)?.let { pending ->
                pending.prompt?.cancelAuthentication()
                reply.postMessage(bioReplyJson(reqId, false, "timeout"))
            }
        }
        prompt = BiometricPrompt(
            this,
            ContextCompat.getMainExecutor(this),
            object : BiometricPrompt.AuthenticationCallback() {
                override fun onAuthenticationSucceeded(result: BiometricPrompt.AuthenticationResult) {
                    val pending = takePendingBiometric(pat, reqId) ?: return
                    mainHandler.removeCallbacks(pending.timeout)
                    val armed = try {
                        if (decision.mode == BiometricMode.StrongCrypto) {
                            // Crypto path: the op must succeed under the biometric-unlocked key —
                            // fail-closed if the result carries no crypto object.
                            result.cryptoObject?.cipher?.doFinal(pat.toByteArray(Charsets.UTF_8))
                                ?: throw IllegalStateException("no crypto object in auth result")
                        }
                        NativeEngine.nativeConfirmAction(pat)
                    } catch (_: Exception) {
                        android.util.Log.w(TAG, "confirmation_arming_failed")
                        false
                    }
                    reply.postMessage(
                        bioReplyJson(reqId, armed, if (armed) "confirmed" else "arming_failed"),
                    )
                }

                override fun onAuthenticationError(code: Int, msg: CharSequence) {
                    val closedCode = BiometricErrorPolicy.closedCode(code)
                    android.util.Log.i(TAG, "native confirmation ended: $closedCode")
                    completeBiometric(pat, reqId, reply, false, closedCode)
                }

                // A single non-match keeps the prompt up; no reply until success/error/cancel.
                override fun onAuthenticationFailed() {}
            },
        )
        val pending = PendingBio(reqId, prompt, timeout, reply)
        if (!bioPending.register(pat, pending)) {
            reply.postMessage(bioReplyJson(reqId, false, "busy"))
            return
        }
        mainHandler.postDelayed(timeout, BIO_TIMEOUT_MS)
        try {
            val info = BiometricPolicy.buildPromptInfo(
                Build.VERSION.SDK_INT,
                decision,
                "Confirm action",
                descriptor.label,
            )
            if (decision.mode == BiometricMode.StrongCrypto) {
                prompt.authenticate(info, BiometricPrompt.CryptoObject(checkNotNull(cipher)))
            } else {
                prompt.authenticate(info)
            }
        } catch (_: Exception) {
            android.util.Log.w(TAG, "biometric_prompt_start_failed")
            completeBiometric(pat, reqId, reply, false, "start_failed")
        }
    }

    @Suppress("DEPRECATION")
    private fun runDeviceCredential(
        reqId: String,
        pat: String,
        label: String,
        reply: JavaScriptReplyProxy,
    ) {
        if (activeDeviceCredential != null) {
            reply.postMessage(bioReplyJson(reqId, false, "busy"))
            return
        }
        val intent = getSystemService(KeyguardManager::class.java)
            ?.createConfirmDeviceCredentialIntent("Confirm action", label)
        if (intent == null) {
            reply.postMessage(bioReplyJson(reqId, false, "not_available"))
            return
        }
        val key = pat to reqId
        val timeout = Runnable {
            if (activeDeviceCredential != key) return@Runnable
            activeDeviceCredential = null
            runCatching { finishActivity(DEVICE_CREDENTIAL_REQUEST_CODE) }
            completeBiometric(pat, reqId, reply, false, "timeout")
        }
        val pending = PendingBio(reqId, null, timeout, reply)
        if (!bioPending.register(pat, pending)) {
            reply.postMessage(bioReplyJson(reqId, false, "busy"))
            return
        }
        activeDeviceCredential = key
        mainHandler.postDelayed(timeout, BIO_TIMEOUT_MS)
        try {
            startActivityForResult(intent, DEVICE_CREDENTIAL_REQUEST_CODE)
        } catch (_: Exception) {
            android.util.Log.w(TAG, "device_credential_prompt_start_failed")
            activeDeviceCredential = null
            completeBiometric(pat, reqId, reply, false, "start_failed")
        }
    }

    @Deprecated("Android activity-result callback required by the system credential intent")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode == ATTACHMENT_DOCUMENT_REQUEST_CODE) {
            completeAttachmentDocument(if (resultCode == Activity.RESULT_OK) data?.data else null)
            return
        }
        if (requestCode != DEVICE_CREDENTIAL_REQUEST_CODE) return
        val (pat, reqId) = activeDeviceCredential ?: return
        activeDeviceCredential = null
        val pending = takePendingBiometric(pat, reqId) ?: return
        mainHandler.removeCallbacks(pending.timeout)
        val armed = if (resultCode == Activity.RESULT_OK) {
            runCatching { NativeEngine.nativeConfirmAction(pat) }.getOrElse {
                android.util.Log.w(TAG, "device_credential_arming_failed")
                false
            }
        } else {
            false
        }
        pending.reply.postMessage(
            bioReplyJson(
                reqId,
                armed,
                if (armed) "confirmed" else if (resultCode == Activity.RESULT_OK) "arming_failed" else "cancelled",
            ),
        )
    }

    private fun completeBiometric(
        pat: String,
        reqId: String,
        reply: JavaScriptReplyProxy,
        ok: Boolean,
        code: String,
    ) {
        val pending = takePendingBiometric(pat, reqId) ?: return
        mainHandler.removeCallbacks(pending.timeout)
        reply.postMessage(bioReplyJson(reqId, ok, code))
    }

    private fun takePendingBiometric(pat: String, reqId: String): PendingBio? {
        return bioPending.take(pat) { it.requestId == reqId }
    }

    private fun bioReplyJson(reqId: String, ok: Boolean, code: String): String =
        JSONObject()
            .put("t", "bio")
            .put("id", reqId)
            .put("ok", ok)
            .put("code", code)
            .toString()

    private fun cancelPendingBiometrics() {
        activeDeviceCredential = null
        runCatching { finishActivity(DEVICE_CREDENTIAL_REQUEST_CODE) }
        bioPending.drain().forEach {
            mainHandler.removeCallbacks(it.timeout)
            it.prompt?.cancelAuthentication()
        }
    }

    /** Drain one push stream, forwarding each event to the WebView until it ends (#0A). */
    private fun runBridgeStream(jsId: String, path: String, capabilityToken: String, reply: JavaScriptReplyProxy) {
        if (sessionToken.isBlank()) {
            reply.postMessage(streamEndJson(jsId))
            return
        }
        val nativeId = NativeEngine.nativeStreamOpen(path, sessionToken, capabilityToken)
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
                val envelope = BridgeMessagePolicy.outboundStreamEventJson(jsId, ev)
                if (envelope == null) {
                    reply.postMessage(streamErrorJson(jsId, "stream_event_rejected"))
                    break
                }
                reply.postMessage(envelope)
            }
        } finally {
            bridgeStreams.remove(jsId)
            NativeEngine.nativeStreamClose(nativeId)
            reply.postMessage(streamEndJson(jsId))
        }
    }

    private fun streamEndJson(jsId: String): String =
        JSONObject().put("t", "end").put("id", jsId).toString()

    private fun streamErrorJson(jsId: String, code: String): String =
        JSONObject()
            .put("t", "evt")
            .put("id", jsId)
            .put("ev", JSONObject().put("event", "error").put("message", code))
            .toString()

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

    /** Activity recreation must not clear application-owned network guard leases. */
    override fun onDestroy() {
        cancelPendingBiometrics()
        bridgeStreams.values.forEach { NativeEngine.nativeStreamClose(it) }
        bridgeStreams.clear()
        bridgeExecutor.shutdownNow()
        super.onDestroy()
    }
}
