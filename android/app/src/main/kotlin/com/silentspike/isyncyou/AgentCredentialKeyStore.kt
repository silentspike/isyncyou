package com.silentspike.isyncyou

import android.os.Build
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyInfo
import android.security.keystore.KeyProperties
import android.util.Log
import java.io.File
import java.security.KeyStore
import java.security.SecureRandom
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.SecretKeyFactory
import javax.crypto.spec.GCMParameterSpec

/**
 * At-rest agent credential key provider (#620).
 *
 * This intentionally does not reuse [BodyKeyStore]: provider credentials have their own
 * Android Keystore wrapping key and wrapped data-key file. Rust needs the raw 32-byte
 * data key in-process for the agent credential store; Android Keystore keeps only the
 * wrapping key non-exportable, and the data key exists on disk only as AES-GCM wrapped
 * bytes (`agent_credential.key`).
 */
object AgentCredentialKeyStore {
    private const val TAG = "iSyncYou"
    internal const val WRAP_ALIAS = "isyncyou-agent-credential-wrap-v1"
    internal const val KEY_FILE = "agent_credential.key"
    internal const val DEBUG_FAIL_FILE = ".debug-fail-agent-credential-key"

    data class Evidence(
        val alias: String,
        val keyFile: String,
        val algorithm: String?,
        val keySize: Int?,
        val insideSecureHardware: Boolean?,
        val securityLevel: String?,
        val metadataUnavailableReason: String?,
    )

    data class Result(val key: ByteArray, val justCreated: Boolean, val evidence: Evidence)

    fun getOrCreate(filesDir: File): Result {
        return try {
            if (BuildConfig.DEBUG && File(filesDir, DEBUG_FAIL_FILE).exists()) {
                throw IllegalStateException("debug injected agent credential key failure")
            }
            val ks = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
            val wrapKey = ensureWrapKey(ks)
            val keyFile = File(filesDir, KEY_FILE)
            val evidence = describeKey(wrapKey, keyFile)
            if (keyFile.exists()) {
                Result(unwrap(wrapKey, keyFile.readBytes()), false, evidence)
            } else {
                val data = ByteArray(32).also { SecureRandom().nextBytes(it) }
                val wrapped = wrap(wrapKey, data)
                val tmp = File(filesDir, "$KEY_FILE.tmp")
                tmp.writeBytes(wrapped)
                if (!tmp.renameTo(keyFile)) {
                    tmp.delete()
                    throw IllegalStateException("could not persist wrapped agent credential key")
                }
                Result(data, true, evidence)
            }
        } catch (e: Exception) {
            Log.e(TAG, "agent_credential_storage_setup_failed")
            throw EncryptedStorageSetupException("Agent credential storage setup failed", e)
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

    @Suppress("DEPRECATION")
    private fun describeKey(key: SecretKey, keyFile: File): Evidence {
        return try {
            val factory = SecretKeyFactory.getInstance(key.algorithm, "AndroidKeyStore")
            val info = factory.getKeySpec(key, KeyInfo::class.java) as KeyInfo
            Evidence(
                alias = WRAP_ALIAS,
                keyFile = keyFile.name,
                algorithm = key.algorithm,
                keySize = info.keySize,
                insideSecureHardware = info.isInsideSecureHardware,
                securityLevel = securityLevelName(info),
                metadataUnavailableReason = null,
            )
        } catch (e: Exception) {
            Evidence(
                alias = WRAP_ALIAS,
                keyFile = keyFile.name,
                algorithm = key.algorithm,
                keySize = null,
                insideSecureHardware = null,
                securityLevel = null,
                metadataUnavailableReason = e.javaClass.simpleName ?: "unavailable",
            )
        }
    }

    private fun securityLevelName(info: KeyInfo): String? {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S) return null
        return when (info.securityLevel) {
            KeyProperties.SECURITY_LEVEL_SOFTWARE -> "software"
            KeyProperties.SECURITY_LEVEL_TRUSTED_ENVIRONMENT -> "trusted_environment"
            KeyProperties.SECURITY_LEVEL_STRONGBOX -> "strongbox"
            else -> "unknown_${info.securityLevel}"
        }
    }
}
