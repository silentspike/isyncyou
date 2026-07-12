package com.silentspike.isyncyou

import android.system.Os
import android.system.OsConstants
import androidx.test.platform.app.InstrumentationRegistry
import java.io.File
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class NetworkPreflightHookInstrumentedTest {
    @Test
    fun networkPreflightHookIsFeatureGatedOwnerOnlyAndOneShot() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        assertTrue("hook APK did not compile the native feature", NativeEngine.nativeNetworkDeviceHooksEnabled())
        val hook = File(context.filesDir, "network-diagnostic-test-hook")
        hook.delete()
        hook.writeText("tls_failed")
        Os.chmod(hook.absolutePath, OsConstants.S_IRUSR or OsConstants.S_IWUSR)

        assertEquals(NetworkDeviceTestHook.TlsFailed, NetworkDeviceTestHook.take(context))
        assertFalse("hook file must be consumed", hook.exists())
        assertNull(NetworkDeviceTestHook.take(context))
    }
}
