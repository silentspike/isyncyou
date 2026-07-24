package com.silentspike.isyncyou

import android.os.Build
import android.os.Bundle
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import androidx.biometric.BiometricManager
import androidx.biometric.BiometricPrompt
import androidx.core.content.ContextCompat
import androidx.fragment.app.FragmentActivity
import java.security.KeyStore
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey

class BiometricPromptTestActivity : FragmentActivity() {
    private val completed = CountDownLatch(1)
    @Volatile private var outcome = "not_started"

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val cipher = createStrongCipher()
        val decision = BiometricDecision(
            BiometricMode.StrongCrypto,
            BiometricManager.Authenticators.BIOMETRIC_STRONG,
        )
        val prompt = BiometricPrompt(
            this,
            ContextCompat.getMainExecutor(this),
            object : BiometricPrompt.AuthenticationCallback() {
                override fun onAuthenticationSucceeded(result: BiometricPrompt.AuthenticationResult) {
                    outcome = runCatching {
                        val encrypted = checkNotNull(result.cryptoObject?.cipher)
                            .doFinal("isyncyou-strong-probe".toByteArray())
                        check(encrypted.isNotEmpty())
                        "success"
                    }.getOrElse { "crypto_failed:${it.javaClass.simpleName}" }
                    completed.countDown()
                }

                override fun onAuthenticationError(code: Int, message: CharSequence) {
                    outcome = "auth_error:$code"
                    completed.countDown()
                }
            },
        )
        val info = BiometricPolicy.buildPromptInfo(
            Build.VERSION.SDK_INT,
            decision,
            "Confirm test action",
            "Strong biometric CryptoObject verification",
        )
        prompt.authenticate(info, BiometricPrompt.CryptoObject(cipher))
    }

    fun awaitOutcome(timeoutSeconds: Long): String {
        completed.await(timeoutSeconds, TimeUnit.SECONDS)
        return outcome
    }

    private fun createStrongCipher(): Cipher {
        check(Build.VERSION.SDK_INT >= Build.VERSION_CODES.R)
        val alias = "isyncyou-test-strong-crypto"
        val store = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        if (!store.containsAlias(alias)) {
            val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
            generator.init(
                KeyGenParameterSpec.Builder(alias, KeyProperties.PURPOSE_ENCRYPT)
                    .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                    .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                    .setUserAuthenticationRequired(true)
                    .setUserAuthenticationParameters(
                        0,
                        KeyProperties.AUTH_BIOMETRIC_STRONG,
                    )
                    .setInvalidatedByBiometricEnrollment(true)
                    .build(),
            )
            generator.generateKey()
        }
        val key = checkNotNull(store.getKey(alias, null) as? SecretKey)
        return Cipher.getInstance("AES/GCM/NoPadding").apply {
            init(Cipher.ENCRYPT_MODE, key)
        }
    }
}
