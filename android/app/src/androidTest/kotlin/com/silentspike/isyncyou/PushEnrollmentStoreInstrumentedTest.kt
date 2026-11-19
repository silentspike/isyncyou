package com.silentspike.isyncyou

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import java.util.UUID

@RunWith(AndroidJUnit4::class)
class PushEnrollmentStoreInstrumentedTest {
    @Test fun brokerSecretsAreEncryptedAndScoped() {
        val context = ApplicationProvider.getApplicationContext<Context>()
        val instance = UUID.randomUUID().toString().replace("-", "")
        try {
            PushEnrollmentStore.put(context, instance, JSONObject().put("password", "private-sentinel").put("stage", "prepared"))
            assertEquals("private-sentinel", PushEnrollmentStore.get(context, instance)?.getString("password"))
            val prefs = context.getSharedPreferences("push_enrollment_v1", Context.MODE_PRIVATE)
            val raw = requireNotNull(prefs.getString(instance, null))
            assertFalse(raw.contains("private-sentinel"))
            val other = UUID.randomUUID().toString().replace("-", "")
            assertTrue(prefs.edit().putString(other, raw).commit())
            try { assertTrue(runCatching { PushEnrollmentStore.get(context, other) }.isFailure) }
            finally { prefs.edit().remove(other).commit() }
        } finally { PushEnrollmentStore.remove(context, instance) }
    }
}
