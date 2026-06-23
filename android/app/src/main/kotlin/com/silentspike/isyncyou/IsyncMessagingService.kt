package com.silentspike.isyncyou

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.os.Build
import androidx.core.app.NotificationCompat
import com.google.firebase.messaging.FirebaseMessagingService
import com.google.firebase.messaging.RemoteMessage

/**
 * Receives FCM pushes (#575) and posts a native notification — wakes even when the
 * app is closed (Google Play Services delivers to [onMessageReceived]). The server
 * side (daemon, #576) sends a message when a backup completes.
 */
class IsyncMessagingService : FirebaseMessagingService() {

    /** New/rotated FCM registration token — the daemon/relay (#576) targets it. */
    override fun onNewToken(token: String) {
        super.onNewToken(token)
        // The token is delivered to the daemon by the web UI's push registration
        // (#576). SECURITY: never log the token value.
    }

    override fun onMessageReceived(message: RemoteMessage) {
        val n = message.notification
        val title = n?.title ?: message.data["title"] ?: "iSyncYou"
        val body = n?.body ?: message.data["body"] ?: "New backup activity"
        ensureChannel(this)
        val tap = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val notif = NotificationCompat.Builder(this, CHANNEL_ID)
            .setSmallIcon(android.R.drawable.stat_notify_sync)
            .setContentTitle(title)
            .setContentText(body)
            .setAutoCancel(true)
            .setContentIntent(tap)
            .setPriority(NotificationCompat.PRIORITY_DEFAULT)
            .build()
        (getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager)
            .notify(message.messageId?.hashCode() ?: 1, notif)
    }

    companion object {
        const val CHANNEL_ID = "isyncyou-sync"

        /** Register the notification channel (Android 8+); a no-op below API 26. */
        fun ensureChannel(ctx: Context) {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                val ch = NotificationChannel(
                    CHANNEL_ID,
                    "Backup activity",
                    NotificationManager.IMPORTANCE_DEFAULT,
                ).apply { description = "Notifications when iSyncYou backs up new items." }
                (ctx.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager)
                    .createNotificationChannel(ch)
            }
        }
    }
}
