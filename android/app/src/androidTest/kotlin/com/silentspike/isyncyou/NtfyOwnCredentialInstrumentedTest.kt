package com.silentspike.isyncyou

import android.accessibilityservice.AccessibilityServiceInfo
import android.os.Bundle
import android.os.SystemClock
import android.view.accessibility.AccessibilityNodeInfo
import androidx.test.platform.app.InstrumentationRegistry
import java.net.URI
import org.junit.Test

/** Debug-only, narrowly scoped native distributor setup; secrets stay in device RAM. */
class NtfyOwnCredentialInstrumentedTest {
    @Test fun configureOwnDistributor() {
        check(BuildConfig.DEBUG) { "debug required" }
        val inst = InstrumentationRegistry.getInstrumentation()
        val args = InstrumentationRegistry.getArguments()
        val account = requireNotNull(args.getString("testAccount"))
        val expectedHost = requireNotNull(args.getString("expectedHost"))
        check(account.matches(Regex("[A-Za-z0-9._-]{1,128}")) && expectedHost.matches(Regex("[A-Za-z0-9.-]{1,253}"))) { "invalid scope" }
        val ctx = inst.targetContext
        check(EngineBootstrap.ensureStarted(ctx.filesDir).isNotEmpty()) { "engine unavailable" }
        val identity = requireNotNull(PushInstallationKey.existing(ctx, account))
        val state = requireNotNull(PushEnrollmentStore.get(ctx, identity.installation))
        check(state.optString("stage") == "credentials_ready" && state.optString("account") == account &&
            state.optString("origin") == NativeEngine.nativeProviderOrigin(ctx.filesDir.absolutePath, account) &&
            state.optLong("expires") > System.currentTimeMillis() / 1000 + 30) { "enrollment unavailable" }
        val endpoint = URI(state.getString("endpoint"))
        check(endpoint.scheme == "https" && endpoint.host == expectedHost && endpoint.port in listOf(-1,443) && endpoint.rawUserInfo == null && endpoint.rawFragment == null) { "endpoint mismatch" }
        val origin = "https://" + expectedHost
        val username = state.getString("username")
        val password = state.getString("password")
        check(username.isNotEmpty() && password.isNotEmpty()) { "credential unavailable" }
        val automation = inst.uiAutomation
        automation.serviceInfo = automation.serviceInfo.apply { flags = flags or AccessibilityServiceInfo.FLAG_REPORT_VIEW_IDS }
        fun nodes(): List<AccessibilityNodeInfo> {
            val root = automation.rootInActiveWindow ?: error("distributor unavailable")
            check(root.packageName?.toString() == "io.heckel.ntfy") { "wrong foreground" }
            val result = mutableListOf<AccessibilityNodeInfo>()
            fun walk(n: AccessibilityNodeInfo) { result.add(n); for (i in 0 until n.childCount) n.getChild(i)?.let(::walk) }
            walk(root); return result
        }
        fun click(n: AccessibilityNodeInfo) {
            var current: AccessibilityNodeInfo? = n
            while (current != null && !current.isClickable) current = current.parent
            check(current?.performAction(AccessibilityNodeInfo.ACTION_CLICK) == true) { "control unavailable" }
            SystemClock.sleep(500)
        }
        fun byText(vararg names: String): AccessibilityNodeInfo = nodes().singleOrNull { it.isVisibleToUser && it.text?.toString() in names } ?: error("control missing")
        fun set(id: String, value: String) {
            val n = nodes().singleOrNull { it.isVisibleToUser && it.viewIdResourceName == "io.heckel.ntfy:id/$id" } ?: error("field unavailable")
            check(n.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, Bundle().apply { putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, value) })) { "field entry failed" }
        }
        val initial = nodes()
        val more = initial.singleOrNull { it.isVisibleToUser && it.contentDescription?.toString() in setOf("More options", "Weitere Optionen") }
        if (more != null) click(more)
        click(byText("Settings", "Einstellungen"))
        click(byText("Manage users", "Benutzer verwalten"))
        // Refuse to edit any existing account for this origin.
        check(nodes().none { it.isVisibleToUser && it.text?.toString() in setOf(expectedHost, origin) }) { "server user already exists" }
        click(byText("Add new user", "Neuen Benutzer hinzufügen"))
        var saved = false
        try {
            set("user_dialog_base_url", origin)
            set("user_dialog_username", username)
            set("user_dialog_password", password)
            SystemClock.sleep(800)
            val save = nodes().singleOrNull { it.isVisibleToUser && it.viewIdResourceName == "io.heckel.ntfy:id/user_dialog_action_save" && it.isEnabled } ?: error("save unavailable")
            click(save)
            check(nodes().none { it.isVisibleToUser && it.viewIdResourceName == "io.heckel.ntfy:id/user_dialog_password" }) { "save unconfirmed" }
            saved = true
            println("WP16_NTFY_OWN_USER_SAVED=true")
        } finally {
            if (!saved) {
                runCatching { set("user_dialog_password", "") }
                runCatching { set("user_dialog_username", "") }
            }
        }
    }
}
