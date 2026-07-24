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

    internal data class BodyKeyMaterial(
        val keyId: Int,
        val key: ByteArray,
        val justCreated: Boolean,
    )

    internal data class AgentCredentialKeyMaterial(val key: ByteArray)

    /**
     * Start-or-reuse the engine and return the session token (`""` if the engine failed to start).
     * Idempotent: the first successful call caches the token; later calls return it without re-running
     * the bootstrap. The caller must invoke this **off the main thread** (it touches disk).
     * Encrypted-storage setup failures throw and MUST happen before nativeStart/local data open.
     */
    @Synchronized
    fun ensureStarted(filesDir: File): String {
        sessionToken?.let { return it }

        val port = loadAndRunStartupSequence(
            loadBodyKey = {
                BodyKeyStore.getOrCreate(filesDir).let {
                    BodyKeyMaterial(it.keyId, it.key, it.justCreated)
                }
            },
            discardCache = {
                discardReproducibleLocalCache(filesDir)
                Log.i(TAG, "body encryption on: discarded plaintext cache (kept auth)")
            },
            loadAgentCredentialKey = {
                AgentCredentialKeyStore.getOrCreate(filesDir).let {
                    AgentCredentialKeyMaterial(it.key)
                }
            },
            installBodyKey = { keyId, key -> NativeEngine.nativeSetBodyKey(keyId, key) == 1 },
            installAgentCredentialKey = { key -> NativeEngine.nativeSetAgentCredentialKey(key) == 1 },
            startEngine = {
                Log.i(TAG, "EngineBootstrap: calling nativeStart")
                NativeEngine.nativeStart(filesDir.absolutePath)
            },
        )
        Log.i(TAG, "EngineBootstrap: nativeStart returned port=$port")
        if (port > 0) {
            val t = NativeEngine.nativeSessionToken()
            sessionToken = t // cache only on success — a failed start stays retryable
            return t
        }
        return "" // don't cache failure; a later caller may retry
    }

    /**
     * Load both unwrapped keys and keep their complete lifetime under one outer wipe.
     * This covers failures before [runNativeStartupSequence], including cache cleanup
     * and loading the second key.
     */
    internal fun loadAndRunStartupSequence(
        loadBodyKey: () -> BodyKeyMaterial,
        discardCache: () -> Unit,
        loadAgentCredentialKey: () -> AgentCredentialKeyMaterial,
        installBodyKey: (Int, ByteArray) -> Boolean,
        installAgentCredentialKey: (ByteArray) -> Boolean,
        startEngine: () -> Int,
    ): Int {
        var body: BodyKeyMaterial? = null
        var agent: AgentCredentialKeyMaterial? = null
        try {
            body = loadBodyKey()
            if (body.justCreated) discardCache()
            agent = loadAgentCredentialKey()
            return runNativeStartupSequence(
                body.keyId,
                body.key,
                agent.key,
                installBodyKey,
                installAgentCredentialKey,
                startEngine,
            )
        } finally {
            body?.key?.fill(0)
            agent?.key?.fill(0)
        }
    }

    /** The only allowed native startup order, with an injectable seam for JVM tests. */
    internal fun runNativeStartupSequence(
        bodyKeyId: Int,
        bodyKey: ByteArray,
        agentCredentialKey: ByteArray,
        installBodyKey: (Int, ByteArray) -> Boolean,
        installAgentCredentialKey: (ByteArray) -> Boolean,
        startEngine: () -> Int,
    ): Int {
        try {
            if (!installBodyKey(bodyKeyId, bodyKey)) {
                throw EncryptedStorageSetupException("Encrypted storage key install failed")
            }
            if (!installAgentCredentialKey(agentCredentialKey)) {
                throw EncryptedStorageSetupException("Agent credential storage key install failed")
            }
            return startEngine()
        } finally {
            java.util.Arrays.fill(bodyKey, 0)
            java.util.Arrays.fill(agentCredentialKey, 0)
        }
    }

    private fun discardReproducibleLocalCache(filesDir: File) {
        val archive = File(filesDir, "archive")
        if (archive.exists()) {
            val entries = archive.listFiles()
                ?: throw EncryptedStorageSetupException("Could not inspect reproducible cache")
            entries.forEach { file ->
                if (!file.name.startsWith(".isyncyou-token") && !file.deleteRecursively()) {
                    throw EncryptedStorageSetupException("Could not discard reproducible cache")
                }
            }
        }
        for (name in listOf("sync", "cache")) {
            val path = File(filesDir, name)
            if (path.exists() && !path.deleteRecursively()) {
                throw EncryptedStorageSetupException("Could not discard reproducible cache")
            }
        }
    }
}
