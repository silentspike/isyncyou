package com.silentspike.isyncyou

import android.content.Context

/**
 * Test-only #640 diagnostic selector. The value originates from a fixed app-private file
 * consumed by JNI; this object only maps the closed result to the owning boundary.
 */
enum class NetworkDeviceTestHook(val wire: String) {
    NoValidatedNetwork("no_validated_network"),
    ConnectTimeout("connect_timeout"),
    TlsFailed("tls_failed"),
    HttpFailed("http_failed"),
    ForegroundGuardUnavailable("foreground_guard_unavailable"),
    ;

    companion object {
        fun take(context: Context): NetworkDeviceTestHook? = try {
            if (!NativeEngine.nativeNetworkDeviceHooksEnabled()) {
                null
            } else {
                // `filesDir` is app-private but can be lazily created on a fresh install.
                // Create only that fixed directory before native code opens the fixed hook file.
                if (!context.filesDir.exists() && !context.filesDir.mkdirs()) {
                    null
                } else {
                    values().firstOrNull {
                        it.wire == NativeEngine.nativeTakeNetworkDeviceTestHook(context.filesDir.absolutePath)
                    }
                }
            }
        } catch (_: UnsatisfiedLinkError) {
            // Product APKs intentionally do not export either JNI test-hook method.
            null
        }
    }
}
