package com.silentspike.isyncyou

import android.os.Build
import androidx.biometric.BiometricManager
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class BiometricPolicyTest {
    @Test
    fun strongEnrollmentWithCipherFailureFailsClosed() {
        assertNull(BiometricPolicy.choose(true, false, true))
    }

    @Test
    fun deviceCredentialIsOnlyUsedWhenStrongBiometricIsUnavailable() {
        val decision = BiometricPolicy.choose(false, false, true)
        assertEquals(BiometricMode.DeviceCredential, decision?.mode)
        assertEquals(BiometricManager.Authenticators.DEVICE_CREDENTIAL, decision?.authenticators)
    }

    @Test
    fun noAvailableFactorFailsClosed() {
        assertNull(BiometricPolicy.choose(false, false, false))
    }

    @Test
    fun strongModeRequiresLegacyNegativeButton() {
        assertTrue(BiometricPolicy.requiresNegativeButton(Build.VERSION_CODES.Q, BiometricMode.StrongCrypto))
        assertTrue(!BiometricPolicy.requiresNegativeButton(Build.VERSION_CODES.R, BiometricMode.StrongCrypto))
    }

    @Test
    fun labelsComeOnlyFromKnownRustEnums() {
        assertEquals("Delete in OneDrive", BiometricLabelPolicy.label("delete", "onedrive"))
        assertEquals("Make folder offline in OneDrive", BiometricLabelPolicy.label("mode-switch-offline-large", "onedrive"))
        assertEquals("Confirm action in Microsoft 365", BiometricLabelPolicy.label("unknown", "unknown"))
    }
}
