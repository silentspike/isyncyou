package com.silentspike.isyncyou

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import com.google.crypto.tink.apps.fixed_webpush.WebPushHybridDecrypt
import org.json.JSONObject
import org.unifiedpush.android.connector.data.PublicKeySet
import org.unifiedpush.android.connector.keys.KeyManager
import java.security.KeyFactory
import java.security.KeyPairGenerator
import java.security.KeyStore
import java.security.SecureRandom
import java.security.interfaces.ECPrivateKey
import java.security.interfaces.ECPublicKey
import java.security.spec.ECGenParameterSpec
import java.security.spec.PKCS8EncodedKeySpec
import java.security.spec.X509EncodedKeySpec
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/** Web Push key material is wrapped together, including the RFC 8291 auth secret. */
internal class WebPushKeyManager(context: Context) : KeyManager {
    private val prefs = context.getSharedPreferences("webpush_keys_v1", Context.MODE_PRIVATE)
    private val lock = LOCK

    override fun generate(instance: String) = synchronized(lock) {
        require(validInstance(instance))
        val pair = KeyPairGenerator.getInstance("EC").apply {
            initialize(ECGenParameterSpec("secp256r1"))
        }.generateKeyPair()
        val auth = ByteArray(16).also { SecureRandom().nextBytes(it) }
        val privateBytes = pair.private.encoded
        try {
            val json = JSONObject().put("v", 1).put("auth", encode(auth))
                .put("public", encode(pair.public.encoded)).put("private", encode(privateBytes))
            val cipher = Cipher.getInstance("AES/GCM/NoPadding")
            cipher.init(Cipher.ENCRYPT_MODE, wrappingKey(prefs.all.isNotEmpty()))
            cipher.updateAAD(instance.toByteArray(Charsets.UTF_8))
            val sealed = cipher.iv + cipher.doFinal(json.toString().toByteArray(Charsets.UTF_8))
            check(prefs.edit().putString(instance, encode(sealed)).commit())
        } finally {
            auth.fill(0)
            privateBytes.fill(0)
        }
    }

    override fun decrypt(instance: String, sealed: ByteArray): ByteArray? = synchronized(lock) {
        runCatching {
            val keys = load(instance) ?: return null
            try {
                WebPushHybridDecrypt.Builder().withAuthSecret(keys.auth)
                    .withRecipientPublicKey(keys.publicKey).withRecipientPrivateKey(keys.privateKey)
                    .build().decrypt(sealed, null)
            } finally { keys.auth.fill(0) }
        }.getOrNull()
    }

    override fun getPublicKeySet(instance: String): PublicKeySet? = synchronized(lock) {
        runCatching {
            val keys = load(instance) ?: return null
            try {
                val point = byteArrayOf(4) + coordinate(keys.publicKey.w.affineX.toByteArray()) +
                    coordinate(keys.publicKey.w.affineY.toByteArray())
                PublicKeySet(encode(point), encode(keys.auth))
            } finally { keys.auth.fill(0) }
        }.getOrNull()
    }

    override fun exists(instance: String): Boolean = synchronized(lock) {
        val keys = runCatching { load(instance) }.getOrNull() ?: return false
        keys.auth.fill(0)
        true
    }

    override fun delete(instance: String) = synchronized(lock) {
        require(validInstance(instance))
        check(prefs.edit().remove(instance).commit())
    }

    private data class Keys(val auth: ByteArray, val publicKey: ECPublicKey, val privateKey: ECPrivateKey)

    private fun load(instance: String): Keys? {
        if (!validInstance(instance)) return null
        val blob = prefs.getString(instance, null)?.let(::decode) ?: return null
        if (blob.size < 29) return null
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.DECRYPT_MODE, wrappingKey(true), GCMParameterSpec(128, blob.copyOfRange(0, 12)))
        cipher.updateAAD(instance.toByteArray(Charsets.UTF_8))
        val plaintext = cipher.doFinal(blob.copyOfRange(12, blob.size))
        try {
            val json = JSONObject(String(plaintext, Charsets.UTF_8))
            require(json.getInt("v") == 1)
            val auth = decode(json.getString("auth"))
            require(auth.size == 16)
            val factory = KeyFactory.getInstance("EC")
            val pub = factory.generatePublic(X509EncodedKeySpec(decode(json.getString("public")))) as ECPublicKey
            val privateBytes = decode(json.getString("private"))
            val priv = try { factory.generatePrivate(PKCS8EncodedKeySpec(privateBytes)) as ECPrivateKey }
                finally { privateBytes.fill(0) }
            return Keys(auth, pub, priv)
        } finally { plaintext.fill(0) }
    }

    private fun wrappingKey(mustExist: Boolean): SecretKey {
        val store = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (store.getEntry(ALIAS, null) as? KeyStore.SecretKeyEntry)?.let { return it.secretKey }
        check(!mustExist) { "Web Push wrapping key unavailable" }
        return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore").apply {
            init(KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE).setKeySize(256).build())
        }.generateKey()
    }

    private fun coordinate(value: ByteArray): ByteArray {
        val raw = if (value.size == 33 && value[0] == 0.toByte()) value.copyOfRange(1, 33) else value
        require(raw.size <= 32)
        return ByteArray(32 - raw.size) + raw
    }

    private fun validInstance(instance: String) = instance.matches(Regex("[A-Za-z0-9_-]{1,128}"))
    private fun encode(bytes: ByteArray) = Base64.encodeToString(bytes, Base64.URL_SAFE or Base64.NO_PADDING or Base64.NO_WRAP)
    private fun decode(text: String) = Base64.decode(text, Base64.URL_SAFE or Base64.NO_PADDING or Base64.NO_WRAP)

    companion object {
        private const val ALIAS = "isyncyou-webpush-wrap-v1"
        private val LOCK = Any()
    }
}
