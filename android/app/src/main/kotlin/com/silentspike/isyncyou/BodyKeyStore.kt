package com.silentspike.isyncyou

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Log
import java.io.File
import java.security.KeyStore
import java.security.SecureRandom
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * At-rest body key provider (#onedrive-mobile 0B).
 *
 * The Rust envelope needs the raw 32-byte data key in-process (AES-256-GCM), but Android
 * Keystore keys are non-extractable by design. So we use a hardware-backed Keystore key
 * only to **wrap** a random data key: the data key lives on disk **only** wrapped
 * (`body.key`), and is unwrapped in the TEE at startup. The plaintext data key exists only
 * in app memory, handed straight to native and then zeroed from the JVM heap.
 */
object BodyKeyStore {
    private const val TAG = "iSyncYou"
    private const val WRAP_ALIAS = "isyncyou-body-wrap-v1"
    private const val KEY_FILE = "body.key"

    /** The stable key id stamped into every sealed blob (bump on rotation; single key today). */
    const val KEY_ID = 1

    /** The unwrapped data key + whether it was just created (→ discard the plaintext cache). */
    data class Result(val keyId: Int, val key: ByteArray, val justCreated: Boolean)

    /**
     * Return the data key, generating + wrapping it on first run. `null` if the Keystore is
     * unavailable — the caller then runs without at-rest body encryption rather than failing.
     */
    fun getOrCreate(filesDir: File): Result? {
        return try {
            val ks = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
            val wrapKey = ensureWrapKey(ks)
            val keyFile = File(filesDir, KEY_FILE)
            if (keyFile.exists()) {
                Result(KEY_ID, unwrap(wrapKey, keyFile.readBytes()), false)
            } else {
                val data = ByteArray(32).also { SecureRandom().nextBytes(it) }
                val wrapped = wrap(wrapKey, data)
                // Atomic-ish write so a crash can't leave a half-written wrapped key.
                val tmp = File(filesDir, "$KEY_FILE.tmp")
                tmp.writeBytes(wrapped)
                if (!tmp.renameTo(keyFile)) {
                    tmp.delete()
                    throw IllegalStateException("could not persist wrapped body key")
                }
                Result(KEY_ID, data, true)
            }
        } catch (e: Exception) {
            Log.e(TAG, "body key setup failed; running without at-rest body encryption", e)
            null
        }
    }

    private fun ensureWrapKey(ks: KeyStore): SecretKey {
        (ks.getEntry(WRAP_ALIAS, null) as? KeyStore.SecretKeyEntry)?.let { return it.secretKey }
        val kg = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        kg.init(
            KeyGenParameterSpec.Builder(
                WRAP_ALIAS,
                KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
            )
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build(),
        )
        return kg.generateKey()
    }

    /** Wrapped blob = 12-byte GCM IV || ciphertext(32) || tag(16). */
    private fun wrap(key: SecretKey, data: ByteArray): ByteArray {
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.ENCRYPT_MODE, key)
        return c.iv + c.doFinal(data)
    }

    private fun unwrap(key: SecretKey, blob: ByteArray): ByteArray {
        val iv = blob.copyOfRange(0, 12)
        val ct = blob.copyOfRange(12, blob.size)
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(128, iv))
        return c.doFinal(ct)
    }
}
