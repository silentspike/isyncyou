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

/** Dormant until a complete, ACL-aware native reminder plan is qualified. */
class ReminderWorker(context: Context, params: WorkerParameters) : CoroutineWorker(context, params) {
    override suspend fun doWork(): Result = withContext(Dispatchers.IO) {
        val tag = inputData.getString("tag") ?: return@withContext Result.failure()
        if (!tag.matches(Regex("[0-9a-f]{64}"))) return@withContext Result.failure()
        // WorkManager stores only the pseudonymous tag. The source record is encrypted.
        val record = runCatching { PushEnrollmentStore.get(applicationContext, "r_$tag") }.getOrNull()
            ?: return@withContext Result.failure()
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
            return@withContext Result.failure()
        }
        val claimed = runCatching {
            JSONObject(NativeEngine.nativeReminderClaim(applicationContext.filesDir.absolutePath, record.toString()))
        }.getOrNull() ?: return@withContext Result.failure()
        val claimStatus = claimed.optString("status")
        when (claimStatus) {
            "delivered" -> return@withContext Result.success()
            "new", "pending" -> if (claimed.optString("notification_tag") != tag) return@withContext Result.failure()
            else -> return@withContext Result.failure()
        }
        val notification = NotificationCompat.Builder(applicationContext, CHANNEL)
            .setSmallIcon(R.drawable.ic_stat_isyncyou).setContentTitle("Calendar reminder")
            .setContentText("A scheduled item needs your attention.").setAutoCancel(true).build()
        val accepted = runCatching {
            // Pending with no active OS entry may mean a crash before dispatch OR
            // a previously displayed notification already dismissed by the user.
            // Do not automatically display it a second time when that is unknown.
            if (claimStatus == "new") manager.notify(tag, NOTIFICATION_ID, notification)
            manager.activeNotifications.any { it.tag == tag && it.id == NOTIFICATION_ID }
        }.getOrDefault(false)
        if (!accepted) return@withContext Result.failure()
        // An OS readback is not evidence that a person saw the notification.
        if (runCatching { NativeEngine.nativeReminderAcknowledge(applicationContext.filesDir.absolutePath, record.toString(), tag) == 1 }
                .getOrDefault(false)) Result.success() else Result.retry()
    }

    companion object {
        private const val CHANNEL = "calendar_reminders"
        private const val NOTIFICATION_ID = 1
    }
}
