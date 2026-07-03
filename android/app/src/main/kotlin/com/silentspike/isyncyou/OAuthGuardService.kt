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

/**
 * A short-lived foreground service that keeps the app process in a foreground state
 * (FGS type `dataSync`) while the system browser is in front for the OAuth round-trip.
 *
 * Why: during subscription sign-in the app hands the authorize URL to the system browser
 * (app → background), then the embedded engine's loopback `/callback` runs the token
 * exchange — a network call to `platform.claude.com` (Claude) or `chatgpt.com` (Codex).
 * Android restricts an app's network while it is backgrounded (`blocked=APP_BACKGROUND`,
 * aggressive on GrapheneOS), so the exchange times out. An active foreground service lifts
 * that restriction for the duration — the adb-free, all-devices replacement for the Doze
 * whitelist used during bring-up. Started right before the browser opens; stopped once the
 * login completes, times out, or is cancelled (see `NavBridge.beginNetworkGuard` /
 * `endNetworkGuard` in [MainActivity] and the poll loops in `app.js`).
 *
 * If POST_NOTIFICATIONS was denied the notification is simply suppressed by the system —
 * the service still runs and still lifts the network restriction, so sign-in is unaffected.
 */
class OAuthGuardService : Service() {

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        ensureChannel(this)
        val notif: Notification = Notification.Builder(this, CHANNEL_ID)
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
        // The lifecycle is driven explicitly by the UI (begin/end around the sign-in
        // round-trip); no need to restart if the system kills us.
        return START_NOT_STICKY
    }

    companion object {
        private const val CHANNEL_ID = "isyncyou_oauth_guard"
        private const val NOTIF_ID = 42

        fun start(ctx: Context) {
            val i = Intent(ctx, OAuthGuardService::class.java)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) ctx.startForegroundService(i)
            else ctx.startService(i)
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
    }
}
