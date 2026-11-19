package com.silentspike.isyncyou

import android.content.Context
import androidx.annotation.Keep

/** Process-local JNI callback; never exposed through the WebView bridge. */
@Keep
class RecoveryKeyBridge(context: Context) {
    private val app = context.applicationContext

    @Keep
    fun existingId(account: String, origin: String): String = runCatching {
        val identity = requireNotNull(PushInstallationKey.existing(app, account))
        val state = requireNotNull(PushEnrollmentStore.get(app, identity.installation))
        require(state.optString("account") == account && state.optString("origin") == origin)
        require(state.optString("stage") in setOf("broker_bound", "verification_pending", "verify_unknown", "active", "renew_unknown"))
        identity.installation
    }.getOrDefault("")

    @Keep
    fun signExisting(account: String, origin: String, installation: String, challenge: ByteArray): String = runCatching {
        require(existingId(account, origin) == installation)
        PushInstallationKey.signExistingForInstallation(app, account, installation, challenge)
    }.getOrDefault("")
}
