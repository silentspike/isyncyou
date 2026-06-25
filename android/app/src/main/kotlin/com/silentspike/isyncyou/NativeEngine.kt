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
}
