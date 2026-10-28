package com.silentspike.isyncyou

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import java.security.KeyPairGenerator
import java.security.KeyStore
import java.security.MessageDigest
import java.security.SecureRandom
import java.security.Signature
import java.security.spec.ECGenParameterSpec

/** Installation proof key for the push broker; private material stays in AndroidKeyStore. */
internal object PushInstallationKey {
    private const val PREFIX = "isyncyou-push-install-v1-"
    private const val PREFS = "push_installations_v1"

    data class Identity(val installation: String, val publicKey: String)

    @Synchronized
    fun existing(context: Context, account: String): Identity? {
        require(account.matches(Regex("[A-Za-z0-9._-]{1,128}")))
        val installation = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).getString(digest(account), null)
            ?: return null
        val store = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        val cert = requireNotNull(store.getCertificate(PREFIX + installation))
        return Identity(installation, encode(cert.publicKey.encoded))
    }

    @Synchronized
    fun getOrCreate(context: Context, account: String): Identity {
        require(account.matches(Regex("[A-Za-z0-9._-]{1,128}")))
        val slot = digest(account)
        val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val saved = prefs.getString(slot, null)
        val installation = saved ?: encode(ByteArray(24).also { SecureRandom().nextBytes(it) })
        require(installation.matches(Regex("[A-Za-z0-9_-]{16,128}")))
        val alias = PREFIX + installation
        val keystore = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        if (!keystore.containsAlias(alias)) {
            check(saved == null) { "Installation key unavailable" }
            val generator = KeyPairGenerator.getInstance(KeyProperties.KEY_ALGORITHM_EC, "AndroidKeyStore")
            generator.initialize(
                KeyGenParameterSpec.Builder(alias, KeyProperties.PURPOSE_SIGN or KeyProperties.PURPOSE_VERIFY)
                    .setAlgorithmParameterSpec(ECGenParameterSpec("secp256r1"))
                    .setDigests(KeyProperties.DIGEST_SHA256).build(),
            )
            generator.generateKeyPair()
            check(prefs.edit().putString(slot, installation).commit())
        }
        val certificate = requireNotNull(keystore.getCertificate(alias))
        return Identity(installation, encode(certificate.publicKey.encoded))
    }

    @Synchronized
    fun sign(context: Context, account: String, challenge: ByteArray): String {
        require(challenge.size in 1..8192)
        val identity = getOrCreate(context, account)
        val keystore = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        val key = requireNotNull(keystore.getKey(PREFIX + identity.installation, null))
        val signature = Signature.getInstance("SHA256withECDSA")
        signature.initSign(key as java.security.PrivateKey)
        signature.update(challenge)
        return encode(signature.sign())
    }

    /** Call only after a confirmed remote revoke. */
    @Synchronized
    fun deleteAfterRevoke(context: Context, account: String) {
        val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val slot = digest(account)
        val installation = prefs.getString(slot, null) ?: return
        KeyStore.getInstance("AndroidKeyStore").apply { load(null); deleteEntry(PREFIX + installation) }
        check(prefs.edit().remove(slot).commit())
    }

    private fun digest(value: String) = encode(MessageDigest.getInstance("SHA-256").digest(value.toByteArray(Charsets.UTF_8)))
    private fun encode(bytes: ByteArray) = Base64.encodeToString(bytes, Base64.URL_SAFE or Base64.NO_PADDING or Base64.NO_WRAP)
}
