package com.silentspike.isyncyou

import android.Manifest
import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat
import androidx.work.CoroutineWorker
import androidx.work.WorkerParameters
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import org.json.JSONObject

internal object ReminderStatus {
    private const val PREFS = "reminder_status_v1"
    fun record(context: Context, status: String, reason: String = "") {
        require(status in setOf("idle", "planning", "ready", "unavailable", "partial_coverage", "unsupported_alarm_coverage"))
        require(reason in setOf("", "reminder_window", "reminder_changed", "reminder_config", "reminder_client", "reminder_discovery", "reminder_sync", "reminder_cache", "reminder_events_report", "reminder_tasks_report", "reminder_read", "reminder_access_check", "reminder_access_denied", "reminder_resource_get", "reminder_plan"))
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit()
            .putString("status", status).putString("reason", reason).putLong("at", System.currentTimeMillis()).apply()
    }
    fun snapshot(context: Context): JSONObject {
        val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val manager = context.getSystemService(NotificationManager::class.java)
        val enabled = NotificationManagerCompat.from(context).areNotificationsEnabled() &&
            (Build.VERSION.SDK_INT < 33 || ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED) &&
            (Build.VERSION.SDK_INT < 26 || manager?.getNotificationChannel("calendar_reminders")?.importance != NotificationManager.IMPORTANCE_NONE)
        val connectivity = context.getSystemService(android.net.ConnectivityManager::class.java)
        val caps = connectivity?.getNetworkCapabilities(connectivity.activeNetwork)
        val online = caps?.hasCapability(android.net.NetworkCapabilities.NET_CAPABILITY_VALIDATED) == true
        return JSONObject().put("planning", prefs.getString("status", "idle"))
            .put("reason", prefs.getString("reason", ""))
            .put("last_checked_at", prefs.getLong("at", 0)).put("notifications_enabled", enabled).put("network_validated", online)
    }
}

/** Rechecks native authorization and reminder identity immediately before posting. */
class ReminderWorker(context: Context, params: WorkerParameters) : CoroutineWorker(context, params) {
    override suspend fun doWork(): Result = withContext(Dispatchers.IO) {
        val tag = inputData.getString("tag") ?: return@withContext Result.failure()
        if (!tag.matches(Regex("[0-9a-f]{64}"))) return@withContext Result.failure()
        // WorkManager stores only the pseudonymous tag. The source record is encrypted.
        val record = runCatching { PushEnrollmentStore.get(applicationContext, "r_$tag") }.getOrNull()
            ?: return@withContext Result.failure()
        val trigger = record.optLong("trigger_at", 0)
        if (trigger <= 0 || trigger < System.currentTimeMillis() / 1000 - 86400) {
            runCatching { PushEnrollmentStore.remove(applicationContext, "r_$tag") }
            return@withContext Result.failure()
        }
        if (!runCatching { EngineBootstrap.ensureStarted(applicationContext.filesDir).isNotEmpty() }.getOrDefault(false)) {
            return@withContext Result.retry()
        }
        val manager = applicationContext.getSystemService(NotificationManager::class.java) ?: return@withContext Result.failure()
        if (Build.VERSION.SDK_INT >= 26 && manager.getNotificationChannel(CHANNEL) == null) {
            manager.createNotificationChannel(NotificationChannel(CHANNEL, "Calendar reminders", NotificationManager.IMPORTANCE_DEFAULT))
        }
        if (!NotificationManagerCompat.from(applicationContext).areNotificationsEnabled() ||
            (Build.VERSION.SDK_INT >= 33 && ContextCompat.checkSelfPermission(applicationContext,
                Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) ||
            (Build.VERSION.SDK_INT >= 26 && manager.getNotificationChannel(CHANNEL)?.importance == NotificationManager.IMPORTANCE_NONE)) {
            return@withContext Result.retry()
        }
        val claimed = runCatching {
            JSONObject(NativeEngine.nativeReminderClaim(applicationContext.filesDir.absolutePath, record.toString()))
        }.getOrNull() ?: return@withContext Result.retry()
        val claimStatus = claimed.optString("status")
        when (claimStatus) {
            "delivered" -> {
                runCatching { PushEnrollmentStore.remove(applicationContext, "r_$tag") }
                return@withContext Result.success()
            }
            "new", "pending" -> if (claimed.optString("notification_tag") != tag) return@withContext Result.failure()
            else -> return@withContext Result.retry()
        }
        val notification = NotificationCompat.Builder(applicationContext, CHANNEL)
            .setSmallIcon(R.drawable.ic_stat_isyncyou).setContentTitle("Calendar reminder")
            .setContentText("A scheduled item needs your attention.").setAutoCancel(true).build()
        val accepted = runCatching {
            // Pending with no active OS entry may mean a crash before dispatch OR
            // a previously displayed notification already dismissed by the user.
            // Do not automatically display it a second time when that is unknown.
            synchronized(PushEnrollmentStore) {
                // Serialize the final local identity check and notification with
                // planner invalidation. Network authorization happened above.
                val current = PushEnrollmentStore.get(applicationContext, "r_$tag")
                if (isStopped || current == null || current.toString() != record.toString()) {
                    false
                } else {
                    if (claimStatus == "new") manager.notify(tag, NOTIFICATION_ID, notification)
                    manager.activeNotifications.any { it.tag == tag && it.id == NOTIFICATION_ID }
                }
            }
        }.getOrDefault(false)
        if (!accepted) return@withContext Result.failure()
        // An OS readback is not evidence that a person saw the notification.
        if (runCatching { NativeEngine.nativeReminderAcknowledge(applicationContext.filesDir.absolutePath, record.toString(), tag) == 1 }
                .getOrDefault(false)) {
            runCatching { PushEnrollmentStore.remove(applicationContext, "r_$tag") }
            Result.success()
        } else Result.retry()
    }

    companion object {
        private const val CHANNEL = "calendar_reminders"
        private const val NOTIFICATION_ID = 1
    }
}
