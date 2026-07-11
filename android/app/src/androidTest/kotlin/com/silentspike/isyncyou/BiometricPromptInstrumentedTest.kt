package com.silentspike.isyncyou

import android.os.Build
import androidx.biometric.BiometricManager
import androidx.test.core.app.ApplicationProvider
import androidx.test.core.app.ActivityScenario
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test

class BiometricPromptInstrumentedTest {
    @Test
    fun pixelStrongBiometricEnrollmentPrecondition() {
        val context = ApplicationProvider.getApplicationContext<android.content.Context>()
        val availability = BiometricManager.from(context).canAuthenticate(
            BiometricManager.Authenticators.BIOMETRIC_STRONG,
        )
        assumeTrue(
            "physical Strong-Crypto success needs an enrolled strong biometric; status=$availability",
            availability == BiometricManager.BIOMETRIC_SUCCESS,
        )
    }

    @Test
    fun api30PlusStrongCryptoPromptBuildsWithCancelButton() {
        assumeTrue(Build.VERSION.SDK_INT >= Build.VERSION_CODES.R)
        val info = BiometricPolicy.buildPromptInfo(
            Build.VERSION.SDK_INT,
            BiometricDecision(
                BiometricMode.StrongCrypto,
                BiometricManager.Authenticators.BIOMETRIC_STRONG,
            ),
            "Confirm action",
            "Delete in OneDrive",
        )
        assertEquals(
            BiometricManager.Authenticators.BIOMETRIC_STRONG,
            info.allowedAuthenticators,
        )
        assertEquals("Cancel", info.negativeButtonText.toString())
        assertTrue(info.isConfirmationRequired)
    }

    @Test
    fun physicalStrongCryptoPromptUnlocksKeystoreCipher() {
        assumeTrue(Build.VERSION.SDK_INT >= Build.VERSION_CODES.R)
        val context = ApplicationProvider.getApplicationContext<android.content.Context>()
        assumeTrue(
            "physical Strong-Crypto success needs an enrolled strong biometric",
            BiometricManager.from(context).canAuthenticate(
                BiometricManager.Authenticators.BIOMETRIC_STRONG,
            ) == BiometricManager.BIOMETRIC_SUCCESS,
        )
        ActivityScenario.launch(BiometricPromptTestActivity::class.java).use { scenario ->
            lateinit var activity: BiometricPromptTestActivity
            scenario.onActivity { launched -> activity = launched }
            val outcome = activity.awaitOutcome(45)
            assertEquals("success", outcome)
        }
    }
}
