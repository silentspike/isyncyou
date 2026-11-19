package com.silentspike.isyncyou

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import org.json.JSONObject
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/** Endpoint capabilities, broker credentials and enrollment state stay encrypted at rest. */
internal object PushEnrollmentStore {
    private const val ALIAS = "isyncyou-push-enrollment-v1"
    private const val PREFS = "push_enrollment_v1"

    @Synchronized
    fun put(context: Context, instance: String, value: JSONObject) {
        require(valid(instance))
        val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.ENCRYPT_MODE, key(prefs.all.isNotEmpty()))
        cipher.updateAAD(instance.toByteArray(Charsets.UTF_8))
        val bytes = value.toString().toByteArray(Charsets.UTF_8)
        require(bytes.size <= 16384)
        val sealed = cipher.iv + cipher.doFinal(bytes)
        check(prefs.edit().putString(instance, Base64.encodeToString(sealed, Base64.NO_WRAP)).commit())
        bytes.fill(0)
    }

    @Synchronized
    fun get(context: Context, instance: String): JSONObject? {
        if (!valid(instance)) return null
        val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val value = prefs.getString(instance, null) ?: return null
        val bytes = Base64.decode(value, Base64.NO_WRAP)
        require(bytes.size in 29..32768)
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.DECRYPT_MODE, key(true), GCMParameterSpec(128, bytes.copyOfRange(0, 12)))
        cipher.updateAAD(instance.toByteArray(Charsets.UTF_8))
        val plaintext = cipher.doFinal(bytes.copyOfRange(12, bytes.size))
        return try { JSONObject(String(plaintext, Charsets.UTF_8)) } finally { plaintext.fill(0) }
    }

    /** Enumerate only bounded pseudonymous reminder keys; values remain encrypted. */
    @Synchronized
    fun reminderKeys(context: Context): List<String> {
        val keys = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).all.keys
            .filter { it.matches(Regex("r_[0-9a-f]{64}")) }
        require(keys.size <= 10000)
        return keys
    }

    @Synchronized
    fun remove(context: Context, instance: String) {
        require(valid(instance))
        check(context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit().remove(instance).commit())
    }

    @Synchronized
    fun update(context: Context, instance: String, change: (JSONObject) -> Boolean): Boolean {
        val state = get(context, instance) ?: return false
        if (!change(state)) return false
        put(context, instance, state)
        return true
    }

    @Synchronized
    fun removeIf(context: Context, instance: String, predicate: (JSONObject) -> Boolean): Boolean {
        val state = get(context, instance) ?: return false
        if (!predicate(state)) return false
        remove(context, instance)
        return true
    }

    private fun key(mustExist: Boolean): SecretKey {
        val ks = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (ks.getEntry(ALIAS, null) as? KeyStore.SecretKeyEntry)?.let { return it.secretKey }
        check(!mustExist) { "Push storage key unavailable" }
        return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore").apply {
            init(KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE).setKeySize(256).build())
        }.generateKey()
    }

    private fun valid(instance: String) = instance.matches(Regex("[A-Za-z0-9_-]{16,128}"))
}
