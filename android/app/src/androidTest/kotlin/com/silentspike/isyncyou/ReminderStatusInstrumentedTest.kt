package com.silentspike.isyncyou

import android.Manifest
import android.content.pm.PackageManager
import android.os.Build
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Test

/** Checks actual OS notification permission without logging account or calendar data. */
class ReminderStatusInstrumentedTest {
    @Test fun reflectsOsNotificationAvailability() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val status = ReminderStatus.snapshot(context)
        val appEnabled = NotificationManagerCompat.from(context).areNotificationsEnabled()
        val permission = Build.VERSION.SDK_INT < 33 ||
            ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED
        if (!appEnabled || !permission) check(!status.getBoolean("notifications_enabled"))
        check(status.getString("planning") in setOf("idle", "planning", "ready", "unavailable", "unsupported_alarm_coverage"))
        check(status.getLong("last_checked_at") >= 0)
        check(status.has("network_validated"))
        println("WP16_REMINDER_NOTIFICATION_PERMISSION=$permission")
        println("WP16_REMINDER_NOTIFICATION_ENABLED=${status.getBoolean("notifications_enabled")}")
    }
}
