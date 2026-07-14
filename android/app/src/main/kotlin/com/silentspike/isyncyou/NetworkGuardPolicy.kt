package com.silentspike.isyncyou

import android.content.ActivityNotFoundException
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.provider.Settings

enum class NetworkGuardReason(
    val wire: String,
    val startingLeaseMs: Long,
    val activeLeaseMs: Long = startingLeaseMs,
) {
    OAUTH("oauth", 10 * 60_000L),
    CREDENTIAL_REFRESH("credential_refresh", 2 * 60_000L),
    CREDENTIAL_REVOKE("credential_revoke", 2 * 60_000L),
    AGENT_TURN("agent_turn", 2 * 60_000L, 20 * 60_000L),
    ;

    companion object {
        fun fromWire(value: String): NetworkGuardReason? = entries.firstOrNull { it.wire == value }
    }
}

enum class NetworkSettingsHint(val wire: String) {
    INTERNET_PANEL("internet_panel"),
    BACKGROUND_DATA("background_data"),
    APP_DETAILS("app_details"),
    BATTERY_SETTINGS("battery_settings"),
    ;

    companion object {
        fun fromWire(value: String): NetworkSettingsHint? = entries.firstOrNull { it.wire == value }
    }
}

object NetworkGuardPolicy {
    const val MAX_LEASES = 8
    const val MAX_TURN_ID_CHARS = 128

    private val turnId = Regex("[A-Za-z0-9._-]{1,$MAX_TURN_ID_CHARS}")

    fun validTurnId(value: String): Boolean = turnId.matches(value)
}

/** Only this policy constructs Settings intents for network diagnostics. */
object NetworkSettingsPolicy {
    enum class Target(val action: String, val needsPackageUri: Boolean) {
        // The platform action is available from API 29; targets() exposes it only on API 29+.
        INTERNET_PANEL("android.settings.panel.action.INTERNET_CONNECTIVITY", false),
        BACKGROUND_DATA(Settings.ACTION_IGNORE_BACKGROUND_DATA_RESTRICTIONS_SETTINGS, true),
        APP_DETAILS(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, true),
        BATTERY_SETTINGS(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS, false),
    }

    /** Pure decision function for local JVM tests. */
    fun targets(hint: NetworkSettingsHint, sdk: Int): List<Target> {
        val primary = when (hint) {
            NetworkSettingsHint.INTERNET_PANEL -> if (sdk >= Build.VERSION_CODES.Q) Target.INTERNET_PANEL else Target.APP_DETAILS
            NetworkSettingsHint.BACKGROUND_DATA -> Target.BACKGROUND_DATA
            NetworkSettingsHint.APP_DETAILS -> Target.APP_DETAILS
            NetworkSettingsHint.BATTERY_SETTINGS -> Target.BATTERY_SETTINGS
        }
        return if (primary == Target.APP_DETAILS) {
            listOf(Target.APP_DETAILS)
        } else {
            listOf(primary, Target.APP_DETAILS)
        }
    }

    private fun intent(target: Target, packageName: String): Intent =
        if (target.needsPackageUri) Intent(target.action, Uri.fromParts("package", packageName, null))
        else Intent(target.action)

    fun open(context: Context, hint: NetworkSettingsHint): Boolean {
        for (target in targets(hint, Build.VERSION.SDK_INT)) {
            val intent = intent(target, context.packageName)
            if (intent.resolveActivity(context.packageManager) == null) continue
            try {
                context.startActivity(intent)
                return true
            } catch (_: ActivityNotFoundException) {
                // A resolver can disappear between the check and launch; try app details once.
            }
        }
        return false
    }
}
