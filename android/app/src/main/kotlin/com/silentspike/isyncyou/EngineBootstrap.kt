package com.silentspike.isyncyou

import android.util.Log
import java.io.File

/**
 * Single, synchronized, idempotent entry point that starts the embedded engine and returns the
 * per-process session token. Extracted from [MainActivity] (#onedrive-mobile 0A/0B) so the headless
 * `DocumentsProvider` (S-OM.12, #658) and the Activity share **one** bootstrap.
 *
 * Correctness — not just DRY: `wipe-if-justCreated` must run **exactly once**. The provider runs on
 * Binder threads and [MainActivity] on the UI thread; both can trigger init. `@Synchronized` makes
 * the first caller run the full sequence (incl. the plaintext-cache wipe) while every other caller
 * gets the cached token, so the wipe can never race a concurrent first start (which would corrupt
 * the store).
 */
object EngineBootstrap {
    private const val TAG = "iSyncYou"

    @Volatile
    private var sessionToken: String? = null

    /**
     * Start-or-reuse the engine and return the session token (`""` if the engine failed to start).
     * Idempotent: the first successful call caches the token; later calls return it without re-running
     * the bootstrap. The caller must invoke this **off the main thread** (it touches disk).
     */
    @Synchronized
    fun ensureStarted(filesDir: File): String {
        sessionToken?.let { return it }

        // Install the at-rest body key from the Keystore BEFORE the engine touches disk (#0B), so
        // the first body write/read is already sealed.
        BodyKeyStore.getOrCreate(filesDir)?.let { r ->
            if (r.justCreated) {
                // First encrypted run: discard the pre-encryption plaintext CACHE (the store DB + body
                // files) so it re-syncs sealed — but KEEP the auth token (also under archive/,
                // `.isyncyou-token*`) so the user stays signed in. The cache is reproducible; the token
                // is not.
                File(filesDir, "archive").listFiles()?.forEach { f ->
                    if (!f.name.startsWith(".isyncyou-token")) f.deleteRecursively()
                }
                File(filesDir, "sync").deleteRecursively()
                File(filesDir, "cache").deleteRecursively()
                Log.i(TAG, "body encryption on: discarded plaintext cache (kept auth)")
            }
            NativeEngine.nativeSetBodyKey(r.keyId, r.key)
            java.util.Arrays.fill(r.key, 0) // wipe the data key from the JVM heap
        }

        Log.i(TAG, "EngineBootstrap: calling nativeStart")
        val port = NativeEngine.nativeStart(filesDir.absolutePath)
        Log.i(TAG, "EngineBootstrap: nativeStart returned port=$port")
        if (port > 0) {
            val t = NativeEngine.nativeSessionToken()
            sessionToken = t // cache only on success — a failed start stays retryable
            return t
        }
        return "" // don't cache failure; a later caller may retry
    }
}
