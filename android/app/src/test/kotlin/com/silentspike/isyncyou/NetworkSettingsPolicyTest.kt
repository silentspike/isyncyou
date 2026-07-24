package com.silentspike.isyncyou

import android.os.Build
import android.provider.Settings
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class NetworkSettingsPolicyTest {
    @Test
    fun allowsOnlyClosedHints() {
        assertEquals(NetworkSettingsHint.APP_DETAILS, NetworkSettingsHint.fromWire("app_details"))
        assertEquals(NetworkSettingsHint.BACKGROUND_DATA, NetworkSettingsHint.fromWire("background_data"))
        assertEquals(null, NetworkSettingsHint.fromWire("intent://attacker"))
    }

    @Test
    fun mapsDataSaverToPackageBackgroundDataSettings() {
        val candidate = NetworkSettingsPolicy.targets(
            NetworkSettingsHint.BACKGROUND_DATA,
            Build.VERSION_CODES.UPSIDE_DOWN_CAKE,
        ).first()
        assertEquals(NetworkSettingsPolicy.Target.BACKGROUND_DATA, candidate)
        assertEquals(Settings.ACTION_IGNORE_BACKGROUND_DATA_RESTRICTIONS_SETTINGS, candidate.action)
        assertTrue(candidate.needsPackageUri)
    }

    @Test
    fun internetPanelFallsBackToAppDetailsOnOlderAndroid() {
        val candidates = NetworkSettingsPolicy.targets(
            NetworkSettingsHint.INTERNET_PANEL,
            Build.VERSION_CODES.P,
        )
        assertEquals(1, candidates.size)
        assertEquals(NetworkSettingsPolicy.Target.APP_DETAILS, candidates.single())
    }

    @Test
    fun batterySettingsUsesGenericListThenAppDetailsFallback() {
        val candidates = NetworkSettingsPolicy.targets(
            NetworkSettingsHint.BATTERY_SETTINGS,
            Build.VERSION_CODES.UPSIDE_DOWN_CAKE,
        )
        assertEquals(NetworkSettingsPolicy.Target.BATTERY_SETTINGS, candidates.first())
        assertEquals(NetworkSettingsPolicy.Target.APP_DETAILS, candidates.last())
        assertEquals(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS, candidates.first().action)
        assertTrue(candidates.last().needsPackageUri)
    }
}
