package com.silentspike.isyncyou

import java.io.File

enum class BridgeStartupDecision {
    Proceed,
    FailForcedDebug,
    FailUnsupported,
    FailRegistration,
}

object BridgeStartupPolicy {
    const val FORCE_FAIL_RELATIVE_PATH = "debug/force_bridge_preflight_fail"

    fun forcedFailureFlag(filesDir: File, debug: Boolean): Boolean =
        debug && File(filesDir, FORCE_FAIL_RELATIVE_PATH).exists()

    fun decide(
        webMessageListenerSupported: Boolean,
        registrationSucceeded: Boolean,
        forcedDebugFailure: Boolean,
    ): BridgeStartupDecision = when {
        forcedDebugFailure -> BridgeStartupDecision.FailForcedDebug
        !webMessageListenerSupported -> BridgeStartupDecision.FailUnsupported
        !registrationSucceeded -> BridgeStartupDecision.FailRegistration
        else -> BridgeStartupDecision.Proceed
    }
}
