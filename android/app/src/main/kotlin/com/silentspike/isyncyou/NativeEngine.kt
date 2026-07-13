package com.silentspike.isyncyou

/**
 * JNI bridge to the embedded Rust engine (`libisyncyou_mobile.so`, #89). The native
 * side runs the real iSyncYou engine in-process. The Android WebView reaches it
 * through trusted native asset calls, the origin-bound message bridge, and native
 * stream handles.
 */
object NativeEngine {
    init {
        System.loadLibrary("isyncyou_mobile")
    }

    /**
     * Start the embedded engine (idempotent) and return a positive ready code, or -1 on
     * failure. [filesDir] is the app's private files directory; the token cache + local
     * store live under it.
     */
    external fun nativeStart(filesDir: String): Int

    /** Return the bounded recoverable mobile-job plan for WorkManager. */
    external fun nativeMobileJobPlan(): String

    /** Validate and execute one versioned WorkManager mobile-job request. */
    external fun nativeRunMobileJob(requestJson: String): String

    /** The per-process session token held by trusted native callers, never by WebView JS. */
    external fun nativeSessionToken(): String

    /**
     * Answer one in-process bridge request (#0A). [requestJson] is the JSON envelope from
     * the WebView's `__isyBridge` (`{t:"req",id,method,path,headers,body}`); the return is
     * the complete reply envelope (`{t:"res",id,status,body}`) to post back verbatim.
     */
    external fun nativeBridgeRequest(requestJson: String): String

    /**
     * Answer one browser-initiated GET subresource (the shell + any img/iframe/viewer) for
     * `shouldInterceptRequest` (#0A). Binary-safe: returns
     * `[status:u16 BE][content_type_len:u16 BE][content_type][body]`.
     */
    external fun nativeAssetRequest(path: String, cookie: String): ByteArray

    /**
     * Answer one app-origin GET using the trusted native session token held by the Activity or
     * DocumentsProvider. No WebView-readable cookie is involved.
     */
    external fun nativeAssetRequestWithSession(path: String, sessionToken: String): ByteArray

    /**
     * Open a bridge push stream (the SSE replacement, #0A) for [path], gated by
     * [sessionToken]. Returns a stream id (>0), or 0 if unknown/unauthorized/not started.
     */
    external fun nativeStreamOpen(path: String, sessionToken: String): Long

    /**
     * Block for the next event on stream [id] (a JSON `{event,data}` object), or "" when the
     * stream ended or was closed. The per-stream forwarding thread loops on this.
     */
    external fun nativeStreamNext(id: Long): String

    /** Close a bridge push stream. */
    external fun nativeStreamClose(id: Long)

    /**
     * Install the at-rest body key (#0B): the 32-byte data key the Android Keystore
     * unwrapped, with its [keyId] for rotation. MUST be called before [nativeStart] so the
     * first body write/read is already sealed. Returns 1 on success, 0 on a bad key length.
     */
    external fun nativeSetBodyKey(keyId: Int, key: ByteArray): Int

    /**
     * Install the at-rest agent credential key (#620): the 32-byte data key the Android
     * Keystore unwrapped for provider credentials. MUST be called before [nativeStart] so
     * the embedded app-host credential resolver uses this process-installed key. Returns
     * 1 on success, 0 on a bad key length or native install failure.
     */
    external fun nativeSetAgentCredentialKey(key: ByteArray): Int

    /**
     * #619 evidence hook: available only when the Rust library is built with
     * `ISY_CARGO_FEATURES=agent-session-kdf-bench`. This is intentionally a direct native
     * instrumentation path, not a WebView, bridge, HTTP, or production UI capability.
     */
    external fun nativeAgentSessionKdfBenchmark(iterations: Int): String

    /**
     * #620 evidence hook: available only when the Rust library is built with
     * `ISY_CARGO_FEATURES=agent-credential-store-self-test`. This is intentionally a direct
     * native instrumentation path, not a WebView, bridge, HTTP, or production UI capability.
     */
    external fun nativeAgentCredentialStoreSelfTest(filesDir: String, sentinel: String): String

    /**
     * #640 test/evidence hook marker. This method is present only in a deliberately
     * feature-enabled test APK and is not callable from WebView, HTTP, or the bridge.
     */
    external fun nativeNetworkDeviceHooksEnabled(): Boolean

    /**
     * #640 test/evidence hook input. Available only in the explicitly feature-enabled
     * native library and never reachable from WebView, HTTP, or the bridge. Native code
     * consumes the fixed app-private file before returning one closed diagnostic category.
     */
    external fun nativeTakeNetworkDeviceTestHook(filesDir: String): String

    /**
     * Arm one real Codex credential refresh in the feature-enabled evidence APK. The native
     * implementation consumes a separate owner-only app-private file and is absent from the
     * default APK; WebView, HTTP, and bridge code cannot call it.
     */
    external fun nativeArmCodexRefreshDeviceTestHook(filesDir: String): Boolean

    /**
     * Return the Rust-owned, bounded descriptor for a pending action without consuming it.
     * The JSON result contains only `status`, `op`, and `service`; status is `ok`, `expired`,
     * or `not_found`.
     */
    external fun nativeDescribePendingAction(pendingId: String): String

    /**
     * Record a successful native `BiometricPrompt` for a pending destructive action
     * (#onedrive-mobile 0.6). Called ONLY from the biometric success callback — the WebView
     * has no path to it, which makes the per-action token a real second factor. Returns true
     * when the pending id was found and armed for consumption.
     */
    external fun nativeConfirmAction(pendingId: String): Boolean

    /**
     * Push the current device transfer conditions (#onedrive-mobile 0.9 / S-OM.9): whether the
     * active network is [metered], the device is [charging], and the [freeBytes] free on the
     * sync volume. Read by the offline pass's policy gate (storage floor / Wi-Fi-only /
     * charging-only). May be called any time; the latest value wins.
     */
    external fun nativeDeviceState(metered: Boolean, charging: Boolean, freeBytes: Long)

    /**
     * Register a one-shot Rust-owned handle for a connectivity snapshot captured only after
     * Kotlin has validated the active foreground guard. The returned value is opaque to JS.
     */
    external fun nativeRegisterNetworkSnapshot(
        guardId: String,
        reason: String,
        activeNetwork: Boolean,
        internetCapability: Boolean,
        validatedCapability: Boolean,
        metered: Boolean,
        restrictBackground: String,
        notificationsVisible: Boolean,
        testHook: String,
    ): String

    /** Invalidate every unconsumed native snapshot bound to an ended guard. */
    external fun nativeInvalidateNetworkGuard(guardId: String)
}
