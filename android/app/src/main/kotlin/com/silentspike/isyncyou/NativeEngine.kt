package com.silentspike.isyncyou

/**
 * JNI bridge to the embedded Rust engine (`libisyncyou_mobile.so`, #89). The native
 * side runs the real iSyncYou engine in-process and serves the web UI over loopback.
 */
object NativeEngine {
    init {
        System.loadLibrary("isyncyou_mobile")
    }

    /**
     * Start the embedded engine (idempotent) and return the bound loopback port, or
     * -1 on failure. [filesDir] is the app's private files directory; the token
     * cache + local store live under it.
     */
    external fun nativeStart(filesDir: String): Int

    /** The per-process session token the WebView must send on every data API call. */
    external fun nativeSessionToken(): String

    /**
     * Answer one in-process bridge request (#0A). [requestJson] is the JSON envelope from
     * the WebView's `__isyBridge` (`{t:"req",id,method,path,headers,body}`); the return is
     * the complete reply envelope (`{t:"res",id,status,body}`) to post back verbatim — no
     * loopback TCP port is used. All parsing lives in Rust; this side is a dumb forwarder.
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
}
