package com.silentspike.isyncyou

import android.os.Build
import androidx.biometric.BiometricManager
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test

class BiometricPromptInstrumentedTest {
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
}
