package com.silentspike.isyncyou

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import androidx.core.app.ServiceCompat
import java.util.UUID
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.TimeUnit

/**
 * Process foreground protection for active provider OAuth, credential refresh/revoke, or
 * a streamed Agent turn. It improves Android execution priority but does not promise that
 * a provider or network path is reachable.
 */
class NetworkCriticalGuardService : Service() {
    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val token = intent?.getStringExtra(EXTRA_START_TOKEN)
        try {
            ensureChannel(this)
            val foregroundType = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC
            } else {
                0
            }
            ServiceCompat.startForeground(
                this,
                NOTIFICATION_ID,
                notificationBuilder(this)
                    .setContentTitle("Network activity")
                    .setContentText("Keeping an active iSyncYou connection available.")
                    .setSmallIcon(R.drawable.ic_stat_isyncyou)
                    .setOngoing(true)
                    .build(),
                foregroundType,
            )
            if (!ackStart(token, StartAck(true))) {
                stopForeground(STOP_FOREGROUND_REMOVE)
                stopSelf(startId)
                return START_NOT_STICKY
            }
        } catch (ex: RuntimeException) {
            android.util.Log.w(TAG, "network guard foreground start failed (${ex.javaClass.simpleName})")
            ackStart(token, StartAck(false, ex.javaClass.simpleName ?: "foreground_failed"))
            stopSelf(startId)
            return START_NOT_STICKY
        }
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        NetworkCriticalGuardRuntime.onServiceDestroyed()
        super.onDestroy()
    }

    companion object {
        private const val TAG = "iSyncYou"
        private const val CHANNEL_ID = "isyncyou_network_guard"
        private const val NOTIFICATION_ID = 42
        private const val EXTRA_START_TOKEN = "com.silentspike.isyncyou.NETWORK_GUARD_START_TOKEN"
        private const val START_TIMEOUT_MS = 3_000L
        private val pendingStarts = ConcurrentHashMap<String, ArrayBlockingQueue<StartAck>>()

        private data class StartAck(val ok: Boolean, val error: String? = null)

        fun start(ctx: Context) {
            val token = UUID.randomUUID().toString()
            val queue = ArrayBlockingQueue<StartAck>(1)
            pendingStarts[token] = queue
            val intent = Intent(ctx, NetworkCriticalGuardService::class.java)
                .putExtra(EXTRA_START_TOKEN, token)
            try {
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                    ctx.startForegroundService(intent)
                } else {
                    @Suppress("DEPRECATION")
                    ctx.startService(intent)
                }
                val ack = try {
                    queue.poll(START_TIMEOUT_MS, TimeUnit.MILLISECONDS)
                } catch (ex: InterruptedException) {
                    Thread.currentThread().interrupt()
                    throw IllegalStateException("foreground_interrupted", ex)
                } ?: throw IllegalStateException("foreground_timeout")
                if (!ack.ok) throw IllegalStateException(ack.error ?: "foreground_failed")
            } catch (ex: RuntimeException) {
                pendingStarts.remove(token)
                stop(ctx)
                throw ex
            }
        }

        fun stop(ctx: Context) {
            ctx.stopService(Intent(ctx, NetworkCriticalGuardService::class.java))
        }

        private fun ensureChannel(ctx: Context) {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                val nm = ctx.getSystemService(NotificationManager::class.java)
                if (nm.getNotificationChannel(CHANNEL_ID) == null) {
                    nm.createNotificationChannel(
                        NotificationChannel(
                            CHANNEL_ID,
                            "Network activity",
                            NotificationManager.IMPORTANCE_LOW,
                        ).apply { description = "Shows active iSyncYou network work." },
                    )
                }
            }
        }

        @Suppress("DEPRECATION")
        private fun notificationBuilder(ctx: Context): Notification.Builder =
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) Notification.Builder(ctx, CHANNEL_ID)
            else Notification.Builder(ctx)

        private fun ackStart(token: String?, ack: StartAck): Boolean {
            if (token.isNullOrBlank()) return false
            return pendingStarts.remove(token)?.offer(ack) == true
        }
    }
}
