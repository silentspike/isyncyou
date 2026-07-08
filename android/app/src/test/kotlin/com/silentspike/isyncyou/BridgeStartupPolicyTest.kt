package com.silentspike.isyncyou

import java.nio.file.Files
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class BridgeStartupPolicyTest {
    @Test
    fun forcedFailureFlagOnlyAppliesInDebugBuilds() {
        val tempDir = Files.createTempDirectory("bridge-startup-test").toFile()
        val flag = tempDir.resolve(BridgeStartupPolicy.FORCE_FAIL_RELATIVE_PATH)
        checkNotNull(flag.parentFile).mkdirs()
        flag.writeText("1")

        assertTrue(BridgeStartupPolicy.forcedFailureFlag(tempDir, debug = true))
        assertFalse(BridgeStartupPolicy.forcedFailureFlag(tempDir, debug = false))
    }

    @Test
    fun startupDecisionFailsClosedBeforeProceeding() {
        val forced = BridgeStartupPolicy.decide(
            webMessageListenerSupported = true,
            registrationSucceeded = true,
            forcedDebugFailure = true,
        )
        val unsupported = BridgeStartupPolicy.decide(
            webMessageListenerSupported = false,
            registrationSucceeded = true,
            forcedDebugFailure = false,
        )
        val registration = BridgeStartupPolicy.decide(
            webMessageListenerSupported = true,
            registrationSucceeded = false,
            forcedDebugFailure = false,
        )
        val proceed = BridgeStartupPolicy.decide(
            webMessageListenerSupported = true,
            registrationSucceeded = true,
            forcedDebugFailure = false,
        )

        assertEquals(BridgeStartupDecision.FailForcedDebug, forced)
        assertEquals(BridgeStartupDecision.FailUnsupported, unsupported)
        assertEquals(BridgeStartupDecision.FailRegistration, registration)
        assertEquals(BridgeStartupDecision.Proceed, proceed)

        assertFalse(BridgeStartupPolicy.shouldStartActivityEngine(forced))
        assertFalse(BridgeStartupPolicy.shouldStartActivityEngine(unsupported))
        assertFalse(BridgeStartupPolicy.shouldStartActivityEngine(registration))
        assertTrue(BridgeStartupPolicy.shouldStartActivityEngine(proceed))
    }
}
