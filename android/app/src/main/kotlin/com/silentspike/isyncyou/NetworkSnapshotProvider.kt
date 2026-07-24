package com.silentspike.isyncyou

import android.content.Context
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import androidx.core.app.NotificationManagerCompat

/** A short-lived native view of connectivity. It is never returned to WebView JavaScript. */
data class NetworkSnapshot(
    val activeNetwork: Boolean,
    val internetCapability: Boolean,
    val validatedCapability: Boolean,
    val metered: Boolean,
    val restrictBackground: String,
    val notificationsVisible: Boolean,
)

object NetworkSnapshotProvider {
    fun capture(context: Context): NetworkSnapshot {
        val manager = context.getSystemService(ConnectivityManager::class.java)
        val network = manager?.activeNetwork
        val capabilities = network?.let { manager.getNetworkCapabilities(it) }
        val restrictBackground = when (manager?.restrictBackgroundStatus) {
            ConnectivityManager.RESTRICT_BACKGROUND_STATUS_DISABLED -> "disabled"
            ConnectivityManager.RESTRICT_BACKGROUND_STATUS_WHITELISTED -> "whitelisted"
            ConnectivityManager.RESTRICT_BACKGROUND_STATUS_ENABLED -> "enabled"
            else -> "unknown"
        }
        return NetworkSnapshot(
            activeNetwork = network != null,
            internetCapability = capabilities?.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET) == true,
            validatedCapability = capabilities?.hasCapability(NetworkCapabilities.NET_CAPABILITY_VALIDATED) == true,
            metered = manager?.isActiveNetworkMetered == true,
            restrictBackground = restrictBackground,
            notificationsVisible = NotificationManagerCompat.from(context).areNotificationsEnabled(),
        )
    }
}
