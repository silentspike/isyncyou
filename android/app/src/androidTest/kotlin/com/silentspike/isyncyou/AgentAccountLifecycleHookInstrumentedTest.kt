package com.silentspike.isyncyou

import android.system.Os
import android.system.OsConstants
import androidx.test.platform.app.InstrumentationRegistry
import java.io.File
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test

class AgentAccountLifecycleHookInstrumentedTest {
    @Test
    fun lifecycleHookIsFeatureGatedOwnerOnlyClosedAndOneShot() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        assumeTrue(
            "requires the account-lifecycle hook APK",
            NativeEngine.nativeAccountLifecycleDeviceHooksEnabled(),
        )
        val hook = File(context.filesDir, "account-lifecycle-test-hook")

        hook.delete()
        hook.writeText("not-a-lifecycle-checkpoint")
        Os.chmod(hook.absolutePath, OsConstants.S_IRUSR or OsConstants.S_IWUSR)
        assertFalse(NativeEngine.nativeArmAccountLifecycleDeviceTestHook(context.filesDir.path))
        assertFalse("invalid hook file must be consumed", hook.exists())

        hook.writeText("force_revoke_timeout")
        Os.chmod(hook.absolutePath, OsConstants.S_IRUSR or OsConstants.S_IWUSR)
        assertTrue(NativeEngine.nativeArmAccountLifecycleDeviceTestHook(context.filesDir.path))
        assertFalse("valid hook file must be consumed before arming", hook.exists())
    }
}
