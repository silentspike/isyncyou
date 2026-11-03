package com.silentspike.isyncyou

import android.net.Uri
import android.accessibilityservice.AccessibilityServiceInfo
import android.os.Bundle
import android.os.SystemClock
import android.os.Process
import android.system.Os
import android.system.OsConstants
import android.view.accessibility.AccessibilityNodeInfo
import androidx.test.platform.app.InstrumentationRegistry
import org.json.JSONObject
import org.junit.Test
import java.io.File
import javax.crypto.Mac
import javax.crypto.spec.SecretKeySpec

/** Explicitly selected debug-only device test; never included in the application APK. */
class ControlledProviderLoginInstrumentedTest {
    /** Read-only shape diagnostic: never prints field contents, URL, account or codes. */
    @Test fun inspectBrowserFormShape() {
        val automation = InstrumentationRegistry.getInstrumentation().uiAutomation
        automation.serviceInfo = automation.serviceInfo.apply { flags = flags or AccessibilityServiceInfo.FLAG_REPORT_VIEW_IDS }
        val root = automation.rootInActiveWindow ?: error("browser window unavailable")
        val nodes = mutableListOf<AccessibilityNodeInfo>()
        fun walk(n: AccessibilityNodeInfo) { nodes.add(n); for (i in 0 until n.childCount) n.getChild(i)?.let(::walk) }
        walk(root)
        val toolbar = nodes.filter { it.viewIdResourceName == "com.vivaldi.browser:id/url_bar" }
        val webViews = nodes.filter { it.className?.toString() == "android.webkit.WebView" && it.isVisibleToUser }
        val webNodes = mutableListOf<AccessibilityNodeInfo>()
        fun walkWeb(n: AccessibilityNodeInfo) { webNodes.add(n); for (i in 0 until n.childCount) n.getChild(i)?.let(::walkWeb) }
        webViews.forEach(::walkWeb)
        val edits = webNodes.filter { it.isVisibleToUser && it.isEditable }
        val shape = JSONObject()
            .put("browser", root.packageName?.toString() == "com.vivaldi.browser")
            .put("toolbar_count", toolbar.size)
            .put("toolbar_full_https", toolbar.singleOrNull()?.text?.toString()?.startsWith("https://") == true)
            .put("webview_count", webViews.size)
            .put("edit_count", edits.size)
            .put("password_count", edits.count { it.isPassword })
            .put("allow_visible", webNodes.any { it.isVisibleToUser && it.text?.toString()?.trim()?.equals("Allow", true) == true })
        val bar = toolbar.singleOrNull()
        if (bar != null && bar.performAction(AccessibilityNodeInfo.ACTION_FOCUS)) {
            SystemClock.sleep(300)
            val focused = automation.rootInActiveWindow
            val found = focused?.findAccessibilityNodeInfosByViewId("com.vivaldi.browser:id/url_bar")?.singleOrNull()
            shape.put("focused_toolbar_full_https", found?.text?.toString()?.startsWith("https://") == true)
            found?.performAction(AccessibilityNodeInfo.ACTION_CLEAR_FOCUS)
        }
        val clickableBar = automation.rootInActiveWindow
            ?.findAccessibilityNodeInfosByViewId("com.vivaldi.browser:id/url_bar")?.singleOrNull()
        if (clickableBar != null && clickableBar.performAction(AccessibilityNodeInfo.ACTION_CLICK)) {
            SystemClock.sleep(300)
            val clicked = automation.rootInActiveWindow
                ?.findAccessibilityNodeInfosByViewId("com.vivaldi.browser:id/url_bar")?.singleOrNull()
            shape.put("clicked_toolbar_full_https", clicked?.text?.toString()?.startsWith("https://") == true)
            clicked?.performAction(AccessibilityNodeInfo.ACTION_CLEAR_FOCUS)
        }
        println("WP16_BROWSER_FORM_SHAPE=$shape")
    }

    @Test fun completeControlledProviderBrowserLogin() {
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        val fixture = File(instrumentation.targetContext.filesDir, "wp16-controlled-login-once.json")
        check(BuildConfig.DEBUG) { "debug build required" }
        val stat = Os.lstat(fixture.path)
        check(OsConstants.S_ISREG(stat.st_mode) && stat.st_uid == Process.myUid() && stat.st_nlink == 1L &&
            stat.st_mode and 0x1ff == 0x180 && stat.st_size in 1L..16384L) { "fixture unavailable" }
        val credentials = try { JSONObject(fixture.readText()) } finally { check(fixture.delete()) { "fixture cleanup failed" } }
        check(credentials.keys().asSequence().toSet() == setOf("username", "password", "totp_secret", "origin")) { "invalid fixture" }
        val user = credentials.getString("username")
        val password = credentials.getString("password")
        val otpUri = Uri.parse(credentials.getString("totp_secret"))
        val origin = Uri.parse(credentials.getString("origin"))
        check(user.isNotBlank() && user.none { it.isISOControl() } && password.isNotEmpty() && otpUri.scheme == "otpauth" &&
            origin.scheme == "https" && !origin.host.isNullOrBlank() && (origin.port == -1 || origin.port == 443) &&
            origin.userInfo == null && (origin.path.isNullOrEmpty() || origin.path == "/") &&
            origin.query == null && origin.fragment == null) { "invalid fixture" }
        check(otpUri.host == "totp" && (otpUri.getQueryParameter("algorithm") ?: "SHA1").equals("SHA1", true) &&
            (otpUri.getQueryParameter("digits") ?: "6") == "6" && (otpUri.getQueryParameter("period") ?: "30") == "30") {
            "unsupported OTP parameters"
        }
        val automation = instrumentation.uiAutomation
        automation.serviceInfo = automation.serviceInfo.apply {
            flags = flags or AccessibilityServiceInfo.FLAG_REPORT_VIEW_IDS
        }
        fun nodes(): List<AccessibilityNodeInfo> {
            val root = automation.rootInActiveWindow ?: return emptyList()
            if (root.packageName?.toString() != "com.vivaldi.browser") return emptyList()
            val all = mutableListOf<AccessibilityNodeInfo>()
            fun walk(n: AccessibilityNodeInfo) { all.add(n); for (i in 0 until n.childCount) n.getChild(i)?.let(::walk) }
            walk(root)
            return all
        }
        fun trusted(ns: List<AccessibilityNodeInfo>) = ns.any {
            var parent = it.parent
            var inWebView = false
            while (parent != null) {
                if (parent.className?.toString() == "android.webkit.WebView") inWebView = true
                parent = parent.parent
            }
            !inWebView && it.viewIdResourceName == "com.vivaldi.browser:id/url_bar" && it.text?.toString()?.let { text ->
                val url = Uri.parse(text)
                url.scheme == "https" && url.host == origin.host &&
                    (url.port == -1 || url.port == 443) && url.userInfo == null && url.path == "/login"
            } == true
        }
        fun waitFor(predicate: (List<AccessibilityNodeInfo>) -> Boolean): List<AccessibilityNodeInfo> {
            val until = SystemClock.elapsedRealtime() + 30_000
            while (SystemClock.elapsedRealtime() < until) {
                val ns = nodes()
                if (trusted(ns) && predicate(ns)) return ns
                SystemClock.sleep(250)
            }
            error("controlled browser form unavailable")
        }
        fun set(node: AccessibilityNodeInfo, value: String) {
            val args = Bundle().apply { putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, value) }
            check(node.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, args)) { "form entry failed" }
        }
        fun edits(ns: List<AccessibilityNodeInfo>): List<AccessibilityNodeInfo> {
            val web = ns.singleOrNull { it.className?.toString() == "android.webkit.WebView" && it.isVisibleToUser }
                ?: return emptyList()
            val all = mutableListOf<AccessibilityNodeInfo>()
            fun walk(n: AccessibilityNodeInfo) {
                if (n.isVisibleToUser && n.isEditable) all.add(n)
                for (i in 0 until n.childCount) n.getChild(i)?.let(::walk)
            }
            walk(web)
            return all
        }
        fun button(ns: List<AccessibilityNodeInfo>, text: String) = ns.singleOrNull {
            it.isVisibleToUser && it.isClickable && it.text?.toString()?.trim()?.equals(text, ignoreCase = true) == true
        }
        var ns = waitFor { edits(it).count { n -> n.isPassword } == 1 && edits(it).count { n -> !n.isPassword } == 1 }
        println("WP16_AUTH_STAGE=origin_verified")
        println("WP16_AUTH_STAGE=login_form_ready")
        check(trusted(nodes())) { "browser origin changed" }
        println("WP16_AUTH_STAGE=credential_entry_started")
        set(edits(ns).single { !it.isPassword }, user)
        check(trusted(nodes())) { "browser origin changed" }
        set(edits(ns).single { it.isPassword }, password)
        println("WP16_AUTH_STAGE=credentials_set")
        check(trusted(nodes())) { "browser origin changed" }
        println("WP16_AUTH_STAGE=password_submit_started")
        check(button(nodes(), "Sign in")?.performAction(AccessibilityNodeInfo.ACTION_CLICK) == true) { "login submit unavailable" }
        ns = waitFor { edits(it).size == 1 && edits(it).none { n -> n.isPassword } }
        println("WP16_AUTH_STAGE=otp_form_ready")
        val secret = checkNotNull(otpUri.getQueryParameter("secret"))
        val alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567"
        var buffer = 0; var bits = 0
        val decoded = mutableListOf<Byte>()
        for (c in secret.uppercase().trimEnd('=')) {
            val v = alphabet.indexOf(c); check(v >= 0) { "invalid OTP" }
            buffer = (buffer shl 5) or v; bits += 5
            if (bits >= 8) { bits -= 8; decoded.add(((buffer shr bits) and 255).toByte()) }
        }
        val remaining = 30_000L - System.currentTimeMillis() % 30_000L
        if (remaining < 3_000L) SystemClock.sleep(remaining + 200L)
        check(trusted(nodes())) { "browser origin changed" }
        val counter = System.currentTimeMillis() / 30_000L
        val message = ByteArray(8) { i -> (counter ushr (56 - i * 8)).toByte() }
        val mac = Mac.getInstance("HmacSHA1"); mac.init(SecretKeySpec(decoded.toByteArray(), "HmacSHA1"))
        val hash = mac.doFinal(message); val off = hash.last().toInt() and 15
        val code = (((hash[off].toInt() and 127) shl 24) or ((hash[off+1].toInt() and 255) shl 16) or
            ((hash[off+2].toInt() and 255) shl 8) or (hash[off+3].toInt() and 255)) % 1_000_000
        println("WP16_AUTH_STAGE=otp_entry_started")
        set(edits(ns).single(), "%06d".format(java.util.Locale.ROOT, code))
        // Stalwart submits automatically when the sixth digit is entered.
        ns = waitFor { button(it, "Allow") != null }
        println("WP16_AUTH_STAGE=consent_seen")
        println("WP16_AUTH_STAGE=consent_submit_started")
        check(button(ns, "Allow")?.performAction(AccessibilityNodeInfo.ACTION_CLICK) == true) { "consent unavailable" }
    }
}
