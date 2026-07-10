package com.silentspike.isyncyou

import android.os.Build
import androidx.biometric.BiometricManager

enum class BiometricMode {
    StrongCrypto,
    DeviceCredential,
}

data class BiometricDecision(
    val mode: BiometricMode,
    val authenticators: Int,
)

/** Pure policy decisions kept outside MainActivity so API-level behavior is unit-testable. */
object BiometricPolicy {
    fun choose(
        _apiLevel: Int,
        strongAvailable: Boolean,
        cryptoAvailable: Boolean,
        credentialAvailable: Boolean,
    ): BiometricDecision? {
        if (strongAvailable) {
            // A strong biometric must always use the crypto-bound path. A Keystore or
            // Cipher failure is not permission to downgrade to a weaker factor.
            if (!cryptoAvailable) return null
            return BiometricDecision(
                BiometricMode.StrongCrypto,
                BiometricManager.Authenticators.BIOMETRIC_STRONG,
            )
        }
        if (!credentialAvailable) return null

        // DEVICE_CREDENTIAL cannot be combined with BIOMETRIC_STRONG on API 24-29.
        // The caller uses the legacy builder on those releases.
        return BiometricDecision(
            BiometricMode.DeviceCredential,
            BiometricManager.Authenticators.DEVICE_CREDENTIAL,
        )
    }

    fun requiresNegativeButton(apiLevel: Int, mode: BiometricMode): Boolean =
        apiLevel < Build.VERSION_CODES.R && mode == BiometricMode.StrongCrypto
}

object BiometricLabelPolicy {
    private val verbs = mapOf(
        "delete" to "Delete",
        "share" to "Share",
        "external-share" to "Share externally",
        "backup" to "Start backup",
        "restore-cloud" to "Restore to cloud",
        "live-write" to "Run Agent write",
        "upload" to "Upload",
        "replace" to "Replace",
        "move-out-of-protected" to "Move out of offline folder",
        "mode-switch-offline-large" to "Make folder offline",
        "conflict-keep-mine" to "Keep local version",
        "bulk" to "Bulk change",
    )
    private val services = mapOf(
        "onedrive" to "OneDrive",
        "backup" to "iSyncYou",
        "agent" to "iSyncYou",
        "mail" to "Mail",
        "calendar" to "Calendar",
        "contacts" to "Contacts",
        "todo" to "To Do",
        "onenote" to "OneNote",
    )

    fun label(op: String, service: String): String {
        return "${verbs[op] ?: "Confirm action"} in ${services[service] ?: "Microsoft 365"}"
    }
}
