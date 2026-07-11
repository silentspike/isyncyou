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
import java.util.UUID
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.TimeUnit

/**
 * A short-lived foreground service that keeps the app process in a foreground state
 * (FGS type `dataSync`) while the system browser is in front for the OAuth round-trip.
 *
 * Why: during subscription sign-in the app hands the authorize URL to the system browser
 * (app → background), then the embedded engine completes the callback/token exchange — a
 * network call to `platform.claude.com` (Claude) or `chatgpt.com` (Codex).
 * Android restricts an app's network while it is backgrounded (`blocked=APP_BACKGROUND`,
 * aggressive on GrapheneOS), so the exchange times out. An active foreground service lifts
 * that restriction for the duration — the adb-free, all-devices replacement for the Doze
 * whitelist used during bring-up. Started right before the browser opens; stopped once the
 * login completes, times out, or is cancelled (see the native bridge guard ops in
 * [MainActivity] and the poll loops in `app.js`).
 *
 * If POST_NOTIFICATIONS was denied the notification is simply suppressed by the system —
 * the service still runs and still lifts the network restriction, so sign-in is unaffected.
 */
class OAuthGuardService : Service() {

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val token = intent?.getStringExtra(EXTRA_START_TOKEN)
        try {
            ensureChannel(this)
            val notif: Notification = notificationBuilder(this)
                .setContentTitle("Signing in…")
                .setContentText("Keeping the connection open to finish sign-in.")
                .setSmallIcon(R.drawable.ic_stat_isyncyou)
                .setOngoing(true)
                .build()
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                startForeground(NOTIF_ID, notif, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
            } else {
                @Suppress("DEPRECATION")
                startForeground(NOTIF_ID, notif)
            }
            ackStart(token, StartAck(true))
        } catch (ex: RuntimeException) {
            android.util.Log.w("iSyncYou", "OAuth guard foreground start failed (${ex.javaClass.simpleName})")
            ackStart(token, StartAck(false, ex.javaClass.simpleName ?: "foreground_failed"))
            stopSelf(startId)
            return START_NOT_STICKY
        }
        // The lifecycle is driven explicitly by the UI (begin/end around the sign-in
        // round-trip); no need to restart if the system kills us.
        return START_NOT_STICKY
    }

    companion object {
        private const val CHANNEL_ID = "isyncyou_oauth_guard"
        private const val NOTIF_ID = 42
        private const val EXTRA_START_TOKEN = "com.silentspike.isyncyou.OAUTH_GUARD_START_TOKEN"
        private const val START_TIMEOUT_MS = 3_000L
        private val pendingStarts = ConcurrentHashMap<String, ArrayBlockingQueue<StartAck>>()

        private data class StartAck(val ok: Boolean, val error: String? = null)

        fun start(ctx: Context) {
            val token = UUID.randomUUID().toString()
            val queue = ArrayBlockingQueue<StartAck>(1)
            pendingStarts[token] = queue
            val i = Intent(ctx, OAuthGuardService::class.java).putExtra(EXTRA_START_TOKEN, token)
            // The guard is started while MainActivity is visible, before the browser
            // handoff. Start as a regular service and promote in onStartCommand(); this
            // avoids Android 12+ foreground-service-start denial for direct
            // startForegroundService calls from WebView callbacks while still running
            // the service in the foreground for the browser round-trip.
            try {
                ctx.startService(i)
                val ack = try {
                    queue.poll(START_TIMEOUT_MS, TimeUnit.MILLISECONDS)
                } catch (ex: InterruptedException) {
                    Thread.currentThread().interrupt()
                    throw IllegalStateException("foreground_interrupted", ex)
                } ?: throw IllegalStateException("foreground_timeout")
                if (!ack.ok) {
                    throw IllegalStateException(ack.error ?: "foreground_failed")
                }
            } catch (ex: RuntimeException) {
                pendingStarts.remove(token)
                throw ex
            }
        }

        fun stop(ctx: Context) {
            ctx.stopService(Intent(ctx, OAuthGuardService::class.java))
        }

        private fun ensureChannel(ctx: Context) {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                val nm = ctx.getSystemService(NotificationManager::class.java)
                if (nm.getNotificationChannel(CHANNEL_ID) == null) {
                    nm.createNotificationChannel(
                        NotificationChannel(
                            CHANNEL_ID,
                            "Sign-in",
                            NotificationManager.IMPORTANCE_LOW,
                        ).apply { description = "Keeps the connection open during sign-in." },
                    )
                }
            }
        }

        @Suppress("DEPRECATION")
        private fun notificationBuilder(ctx: Context): Notification.Builder =
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                Notification.Builder(ctx, CHANNEL_ID)
            } else {
                Notification.Builder(ctx)
            }

        private fun ackStart(token: String?, ack: StartAck) {
            if (token.isNullOrBlank()) return
            pendingStarts.remove(token)?.offer(ack)
        }
    }
}
