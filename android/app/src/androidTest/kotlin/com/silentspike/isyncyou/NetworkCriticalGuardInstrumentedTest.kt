package com.silentspike.isyncyou

import android.content.ComponentName
import android.content.pm.ServiceInfo
import android.os.Build
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test

class NetworkCriticalGuardInstrumentedTest {
    @Test
    fun networkCriticalGuardStartsAndStopsDeclaredDataSyncService() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val component = ComponentName(context, NetworkCriticalGuardService::class.java)
        val service = if (Build.VERSION.SDK_INT >= 33) {
            context.packageManager.getServiceInfo(
                component,
                android.content.pm.PackageManager.ComponentInfoFlags.of(0),
            )
        } else {
            @Suppress("DEPRECATION")
            context.packageManager.getServiceInfo(component, 0)
        }
        assertNotNull(service)
        if (Build.VERSION.SDK_INT >= 29) {
            assertTrue(
                "network guard service must declare dataSync",
                service.foregroundServiceType and ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC != 0,
            )
        }

        NetworkCriticalGuardRuntime.initialize(context)
        val result = NetworkCriticalGuardRuntime.begin(NetworkGuardReason.OAUTH)
        assertTrue("guard did not start: ${result.error}", result.ok)
        assertEquals(1, NetworkCriticalGuardRuntime.activeCountForTests())
        assertTrue(NetworkCriticalGuardRuntime.end(result.guardId))
        assertEquals(0, NetworkCriticalGuardRuntime.activeCountForTests())
    }
}
