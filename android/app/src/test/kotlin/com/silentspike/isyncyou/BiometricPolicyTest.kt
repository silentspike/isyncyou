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
    fun userPresenceAlwaysUsesDeviceCredentialWhenAvailable() {
        val decision = BiometricPolicy.chooseForOperation(
            operation = "user-presence",
            strongAvailable = true,
            cryptoAvailable = true,
            credentialAvailable = true,
        )
        assertEquals(BiometricMode.DeviceCredential, decision?.mode)
        assertEquals(BiometricManager.Authenticators.DEVICE_CREDENTIAL, decision?.authenticators)
    }

    @Test
    fun userPresenceFailsClosedWithoutDeviceCredential() {
        assertNull(
            BiometricPolicy.chooseForOperation(
                operation = "user-presence",
                strongAvailable = true,
                cryptoAvailable = true,
                credentialAvailable = false,
            ),
        )
    }

    @Test
    fun destructiveOperationRetainsStrongCryptoPreference() {
        val decision = BiometricPolicy.chooseForOperation(
            operation = "delete",
            strongAvailable = true,
            cryptoAvailable = true,
            credentialAvailable = true,
        )
        assertEquals(BiometricMode.StrongCrypto, decision?.mode)
    }

    @Test
    fun authenticationErrorsMapOnlyToClosedCodes() {
        assertEquals(
            "cancelled",
            BiometricErrorPolicy.closedCode(androidx.biometric.BiometricPrompt.ERROR_USER_CANCELED),
        )
        assertEquals(
            "lockout",
            BiometricErrorPolicy.closedCode(androidx.biometric.BiometricPrompt.ERROR_LOCKOUT),
        )
        assertEquals(
            "timeout",
            BiometricErrorPolicy.closedCode(androidx.biometric.BiometricPrompt.ERROR_TIMEOUT),
        )
        assertEquals(
            "not_available",
            BiometricErrorPolicy.closedCode(androidx.biometric.BiometricPrompt.ERROR_HW_UNAVAILABLE),
        )
        assertEquals("authentication_failed", BiometricErrorPolicy.closedCode(Int.MAX_VALUE))
    }

    @Test
    fun noAvailableFactorFailsClosed() {
        assertNull(BiometricPolicy.choose(false, false, false))
    }

    @Test
    fun strongModeAlwaysRequiresNegativeButton() {
        assertTrue(BiometricPolicy.requiresNegativeButton(BiometricMode.StrongCrypto))
        assertTrue(!BiometricPolicy.requiresNegativeButton(BiometricMode.DeviceCredential))
    }

    @Test
    fun legacyDeviceCredentialUsesKeyguardSecurityState() {
        assertTrue(BiometricPolicy.credentialAvailable(Build.VERSION_CODES.Q, false, true))
        assertTrue(!BiometricPolicy.credentialAvailable(Build.VERSION_CODES.Q, true, false))
        assertTrue(BiometricPolicy.credentialAvailable(Build.VERSION_CODES.R, true, false))
    }

    @Test
    fun labelsComeOnlyFromKnownRustEnums() {
        assertEquals("Delete in OneDrive", BiometricLabelPolicy.label("delete", "onedrive"))
        assertEquals("Make folder offline in OneDrive", BiometricLabelPolicy.label("mode-switch-offline-large", "onedrive"))
        assertNull(BiometricLabelPolicy.label("unknown", "unknown"))
        assertNull(BiometricLabelPolicy.label("delete", "unknown"))
    }

    @Test
    fun duplicatePendingHandleCannotOpenTwoPrompts() {
        val registry = BiometricPendingRegistry<String>()
        assertTrue(registry.register("pending-1", "request-1"))
        assertTrue(!registry.register("pending-1", "request-2"))
        assertNull(registry.take("pending-1") { it == "request-2" })
        assertEquals("request-1", registry.take("pending-1") { it == "request-1" })
    }
}
