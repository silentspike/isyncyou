package com.silentspike.isyncyou

import android.os.Build
import androidx.biometric.BiometricManager
import androidx.biometric.BiometricPrompt
import java.util.concurrent.ConcurrentHashMap

enum class BiometricMode {
    StrongCrypto,
    DeviceCredential,
}

data class BiometricDecision(
    val mode: BiometricMode,
    val authenticators: Int,
)

internal class BiometricPendingRegistry<T : Any> {
    private val pending = ConcurrentHashMap<String, T>()

    fun register(handle: String, value: T): Boolean = pending.putIfAbsent(handle, value) == null

    fun take(handle: String, matches: (T) -> Boolean): T? {
        val value = pending[handle] ?: return null
        if (!matches(value) || !pending.remove(handle, value)) return null
        return value
    }

    fun drain(): List<T> {
        val values = pending.values.toList()
        pending.clear()
        return values
    }
}

/** Pure policy decisions kept outside MainActivity so API-level behavior is unit-testable. */
object BiometricPolicy {
    fun choose(
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

    fun credentialAvailable(
        apiLevel: Int,
        biometricManagerAvailable: Boolean,
        keyguardDeviceSecure: Boolean,
    ): Boolean = if (apiLevel >= Build.VERSION_CODES.R) {
        biometricManagerAvailable
    } else {
        keyguardDeviceSecure
    }

    fun requiresNegativeButton(mode: BiometricMode): Boolean =
        mode == BiometricMode.StrongCrypto

    @Suppress("DEPRECATION")
    fun buildPromptInfo(
        apiLevel: Int,
        decision: BiometricDecision,
        title: String,
        subtitle: String,
    ): BiometricPrompt.PromptInfo {
        val builder = BiometricPrompt.PromptInfo.Builder()
            .setTitle(title)
            .setSubtitle(subtitle)
        if (decision.mode == BiometricMode.DeviceCredential && apiLevel < Build.VERSION_CODES.R) {
            builder.setDeviceCredentialAllowed(true)
        } else {
            builder.setAllowedAuthenticators(decision.authenticators)
        }
        if (requiresNegativeButton(decision.mode)) {
            builder.setNegativeButtonText("Cancel")
        }
        return builder.build()
    }
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
        "user-presence" to "Confirm session action",
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

    fun label(op: String, service: String): String? {
        val verb = verbs[op] ?: return null
        val serviceName = services[service] ?: return null
        return "$verb in $serviceName"
    }
}
