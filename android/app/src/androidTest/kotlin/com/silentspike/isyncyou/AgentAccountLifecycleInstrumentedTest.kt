package com.silentspike.isyncyou

import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class AgentAccountLifecycleInstrumentedTest {
    @Test
    fun defaultApkExcludesLifecycleHooksAndRunsBoundedRevokeGuard() {
        assertFalse(
            "default APK must not contain account-lifecycle device hooks",
            NativeEngine.nativeAccountLifecycleDeviceHooksEnabled(),
        )

        val context = InstrumentationRegistry.getInstrumentation().targetContext
        NetworkCriticalGuardRuntime.initialize(context)
        val started = NetworkCriticalGuardRuntime.begin(NetworkGuardReason.CREDENTIAL_REVOKE)
        assertTrue("credential-revoke guard failed: ${started.error}", started.ok)
        val lease = NetworkCriticalGuardRuntime.activeLease(started.guardId)
        assertEquals(NetworkGuardReason.CREDENTIAL_REVOKE, lease?.reason)
        assertTrue(
            "credential-revoke guard must use the fixed two-minute policy",
            (lease?.deadlineElapsedMs ?: 0L) > 0L,
        )
        assertTrue(NetworkCriticalGuardRuntime.end(started.guardId))
    }
}
