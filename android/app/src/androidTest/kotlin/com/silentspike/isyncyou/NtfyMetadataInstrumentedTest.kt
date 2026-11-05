package com.silentspike.isyncyou

import android.system.Os
import android.system.OsConstants
import androidx.test.platform.app.InstrumentationRegistry
import android.view.accessibility.AccessibilityNodeInfo
import java.io.File
import java.io.FileOutputStream
import java.security.SecureRandom
import javax.crypto.Mac
import javax.crypto.spec.SecretKeySpec
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Test

/** Visible-row metadata only. Never emit subscription labels, topics or credentials. */
class NtfyMetadataInstrumentedTest {
    @Test fun verifyVisibleSubscriptions() {
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        val automation = instrumentation.uiAutomation
        automation.serviceInfo = automation.serviceInfo.apply {
            flags = flags or android.accessibilityservice.AccessibilityServiceInfo.FLAG_REPORT_VIEW_IDS
        }
        val root = requireNotNull(automation.rootInActiveWindow)
        check(root.packageName?.toString() == "io.heckel.ntfy") { "distributor not foreground" }
        val nodes = mutableListOf<AccessibilityNodeInfo>()
        fun walk(n: AccessibilityNodeInfo) { nodes.add(n); for (i in 0 until n.childCount) n.getChild(i)?.let(::walk) }
        walk(root)
        val names = nodes.filter { it.isVisibleToUser && it.viewIdResourceName == "io.heckel.ntfy:id/main_item_text" }
        val unrelated = names.filter { n ->
            val parent = n.parent ?: return@filter true
            (0 until parent.childCount).mapNotNull { parent.getChild(it) }
                .none { it.viewIdResourceName == "io.heckel.ntfy:id/main_item_status" &&
                    it.text?.toString()?.contains("com.silentspike.isyncyou.debug") == true }
        }
        val baseline = JSONObject(File(instrumentation.targetContext.filesDir, "wp16-ntfy-visible-baseline.json").readText())
        check(baseline.getInt("version") == 1) { "baseline unavailable" }
        val key = android.util.Base64.decode(baseline.getString("key"), android.util.Base64.NO_WRAP)
        val mac = Mac.getInstance("HmacSHA256"); mac.init(SecretKeySpec(key, "HmacSHA256")); key.fill(0)
        val actual = unrelated.map { n -> mac.doFinal((n.text?.toString() ?: "").toByteArray()).joinToString("") { "%02x".format(it) } }.sorted()
        val stored = baseline.getJSONArray("digests")
        val expected = (0 until stored.length()).map { stored.getString(it) }.sorted()
        val complete = nodes.none { it.isVisibleToUser && it.isScrollable }
        println("WP16_NTFY_UNRELATED_UNCHANGED=${complete && actual == expected}")
        check(complete && actual == expected) { "unrelated subscriptions changed or incomplete" }
    }

    @Test fun captureDefaultServerDisplay() {
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        val automation = instrumentation.uiAutomation
        automation.serviceInfo = automation.serviceInfo.apply {
            flags = flags or android.accessibilityservice.AccessibilityServiceInfo.FLAG_REPORT_VIEW_IDS
        }
        fun nodes(): List<AccessibilityNodeInfo> {
            val root = automation.rootInActiveWindow ?: return emptyList()
            if (root.packageName?.toString() != "io.heckel.ntfy") return emptyList()
            val out = mutableListOf<AccessibilityNodeInfo>()
            fun walk(n: AccessibilityNodeInfo) { out.add(n); for (i in 0 until n.childCount) n.getChild(i)?.let(::walk) }
            walk(root); return out
        }
        var all = nodes()
        val titles = setOf("Default server", "Standardserver", "Standard-Server")
        if (all.none { it.isVisibleToUser && it.text?.toString() in titles + setOf("Notifications", "Benachrichtigungen") }) {
            val overflow = all.singleOrNull { it.isVisibleToUser && it.isClickable &&
                it.contentDescription?.toString() in setOf("More options", "Weitere Optionen") }
            if (overflow != null) { check(overflow.performAction(AccessibilityNodeInfo.ACTION_CLICK)); android.os.SystemClock.sleep(300) }
            all = nodes()
            val settings = all.singleOrNull { it.isVisibleToUser && it.text?.toString() in setOf("Settings", "Einstellungen") }
                ?: error("settings unavailable")
            var clickable: AccessibilityNodeInfo? = settings
            while (clickable != null && !clickable.isClickable) clickable = clickable.parent
            check(clickable?.performAction(AccessibilityNodeInfo.ACTION_CLICK) == true) { "settings unavailable" }
            android.os.SystemClock.sleep(600)
            all = nodes()
        }
        repeat(6) {
            if (all.any { it.isVisibleToUser && it.text?.toString() in titles }) return@repeat
            val scroll = all.firstOrNull { it.isVisibleToUser && it.isScrollable } ?: return@repeat
            if (!scroll.performAction(AccessibilityNodeInfo.ACTION_SCROLL_FORWARD)) return@repeat
            android.os.SystemClock.sleep(250)
            all = nodes()
        }
        val title = all.singleOrNull { it.isVisibleToUser && it.text?.toString() in titles }
            ?: error("default server preference unavailable")
        val parent = requireNotNull(title.parent)
        val summaries = mutableListOf<String>()
        fun scan(n: AccessibilityNodeInfo) {
            if (n.isVisibleToUser && n.viewIdResourceName == "android:id/summary") n.text?.toString()?.let(summaries::add)
            for (i in 0 until n.childCount) n.getChild(i)?.let(::scan)
        }
        scan(parent)
        check(summaries.size == 1) { "default server summary unavailable" }
        var row: AccessibilityNodeInfo? = title
        while (row != null && !row.isClickable) row = row.parent
        check(row?.performAction(AccessibilityNodeInfo.ACTION_CLICK) == true) { "default server dialog unavailable" }
        android.os.SystemClock.sleep(500)
        val field = nodes().singleOrNull { it.isVisibleToUser && it.viewIdResourceName == "io.heckel.ntfy:id/default_server_dialog_url" }
            ?: error("default server field unavailable")
        val rawDefault = field.text?.toString().orEmpty()
        val body = rawDefault.toByteArray()
        val path = File(instrumentation.targetContext.filesDir, "wp16-ntfy-default-display-once.txt")
        val fd = Os.open(path.path, OsConstants.O_WRONLY or OsConstants.O_CREAT or OsConstants.O_EXCL or OsConstants.O_NOFOLLOW, 0x180)
        try { FileOutputStream(fd).use {
            val stat = Os.fstat(fd)
            check(OsConstants.S_ISREG(stat.st_mode) && stat.st_uid == android.os.Process.myUid() &&
                stat.st_nlink == 1L && stat.st_mode and 0x1ff == 0x180) { "private snapshot unavailable" }
            it.write(body); it.fd.sync()
        } } finally { body.fill(0) }
        val window = android.os.ParcelFileDescriptor.AutoCloseInputStream(
            automation.executeShellCommand("dumpsys window")
        ).bufferedReader().use { it.readText() }
        if (window.lineSequence().firstOrNull { "type=ime frame=" in it }?.contains("visible=true") == true) {
            check(automation.performGlobalAction(android.accessibilityservice.AccessibilityService.GLOBAL_ACTION_BACK))
            android.os.SystemClock.sleep(200)
        }
        check(automation.performGlobalAction(android.accessibilityservice.AccessibilityService.GLOBAL_ACTION_BACK))
        println("WP16_NTFY_DEFAULT_CAPTURED=true")
    }

    @Test fun snapshotVisibleSubscriptions() {
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        val automation = instrumentation.uiAutomation
        automation.serviceInfo = automation.serviceInfo.apply {
            flags = flags or android.accessibilityservice.AccessibilityServiceInfo.FLAG_REPORT_VIEW_IDS
        }
        val root = requireNotNull(automation.rootInActiveWindow)
        check(root.packageName?.toString() == "io.heckel.ntfy") { "distributor not foreground" }
        val nodes = mutableListOf<AccessibilityNodeInfo>()
        fun walk(n: AccessibilityNodeInfo) { nodes.add(n); for (i in 0 until n.childCount) n.getChild(i)?.let(::walk) }
        walk(root)
        val names = nodes.filter { it.isVisibleToUser && it.viewIdResourceName == "io.heckel.ntfy:id/main_item_text" }
        val statuses = nodes.filter { it.isVisibleToUser && it.viewIdResourceName == "io.heckel.ntfy:id/main_item_status" }
        val key = ByteArray(32).also { SecureRandom().nextBytes(it) }
        val mac = Mac.getInstance("HmacSHA256"); mac.init(SecretKeySpec(key, "HmacSHA256"))
        val unrelated = names.filter { n ->
            val parent = n.parent ?: return@filter true
            (0 until parent.childCount).mapNotNull { parent.getChild(it) }
                .none { it.viewIdResourceName == "io.heckel.ntfy:id/main_item_status" &&
                    it.text?.toString()?.contains("com.silentspike.isyncyou.debug") == true }
        }
        val digests = unrelated.map { n -> mac.doFinal((n.text?.toString() ?: "").toByteArray()).joinToString("") { "%02x".format(it) } }.sorted()
        val payload = JSONObject().put("version", 1).put("key", android.util.Base64.encodeToString(key, android.util.Base64.NO_WRAP))
            .put("digests", JSONArray(digests)).toString().toByteArray()
        val path = File(instrumentation.targetContext.filesDir, "wp16-ntfy-visible-baseline.json")
        val fd = Os.open(path.path, OsConstants.O_WRONLY or OsConstants.O_CREAT or OsConstants.O_EXCL or OsConstants.O_NOFOLLOW, 0x180)
        try { FileOutputStream(fd).use { it.write(payload); it.fd.sync() } } finally { payload.fill(0); key.fill(0) }
        val out = JSONObject().put("visible_rows", names.size)
            .put("visible_unifiedpush", statuses.count { it.text?.toString()?.contains("(UnifiedPush)") == true })
            .put("scrollable", nodes.any { it.isVisibleToUser && it.isScrollable })
        println("WP16_NTFY_METADATA=$out")
    }
}
