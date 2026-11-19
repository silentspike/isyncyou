package com.silentspike.isyncyou

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.AccessibilityServiceInfo
import android.os.SystemClock
import android.view.accessibility.AccessibilityNodeInfo
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Test

/** Read only: enter a foreign topic only if the visible unread count is already zero. */
class NtfyForeignOriginInstrumentedTest {
    @Test fun verifyForeignOrigins() {
        check(BuildConfig.DEBUG) { "debug required" }
        val i = InstrumentationRegistry.getInstrumentation()
        val expected = requireNotNull(InstrumentationRegistry.getArguments().getString("expectedHost"))
        check(expected.matches(Regex("[A-Za-z0-9.-]{1,253}"))) { "invalid scope" }
        val ui = i.uiAutomation
        ui.serviceInfo = ui.serviceInfo.apply { flags = flags or AccessibilityServiceInfo.FLAG_REPORT_VIEW_IDS }
        fun nodes(): List<AccessibilityNodeInfo> {
            val root = ui.rootInActiveWindow ?: error("distributor unavailable")
            check(root.packageName?.toString() == "io.heckel.ntfy") { "wrong foreground" }
            val result = mutableListOf<AccessibilityNodeInfo>()
            fun walk(n: AccessibilityNodeInfo) { result.add(n); for (x in 0 until n.childCount) n.getChild(x)?.let(::walk) }
            walk(root); return result
        }
        fun descendants(n: AccessibilityNodeInfo): List<AccessibilityNodeInfo> {
            val result = mutableListOf<AccessibilityNodeInfo>()
            fun walk(x: AccessibilityNodeInfo) { result.add(x); for (j in 0 until x.childCount) x.getChild(j)?.let(::walk) }
            walk(n); return result
        }
        var checked = 0
        for (index in 0..1) {
            val all = nodes()
            check(all.none { it.isVisibleToUser && it.isScrollable }) { "incomplete list" }
            val rows = all.filter { it.isVisibleToUser && it.viewIdResourceName == "io.heckel.ntfy:id/main_item_text" }
                .map { requireNotNull(it.parent) }
                .filter { row -> descendants(row).none { it.viewIdResourceName == "io.heckel.ntfy:id/main_item_status" && it.text?.toString()?.contains("com.silentspike.isyncyou.debug") == true } }
            check(rows.size == 2) { "subscription scope changed" }
            val row = rows[index]
            check(descendants(row).none { it.isVisibleToUser && it.viewIdResourceName == "io.heckel.ntfy:id/main_item_new" }) { "unread state would change" }
            var clickable: AccessibilityNodeInfo? = row
            while (clickable != null && !clickable.isClickable) clickable = clickable.parent
            check(clickable?.performAction(AccessibilityNodeInfo.ACTION_CLICK) == true) { "detail unavailable" }
            SystemClock.sleep(600)
            val toolbar = nodes().singleOrNull { it.isVisibleToUser && it.viewIdResourceName == "io.heckel.ntfy:id/toolbar" } ?: error("detail toolbar unavailable")
            val titles = descendants(toolbar).filter { it.isVisibleToUser && it.className?.toString() == "android.widget.TextView" }
                .mapNotNull { it.text?.toString() }.filter { "/" in it }
            // DetailActivity title is topicShortUrl(baseUrl, topic); require its presence.
            check(titles.size == 1) { "topic origin unavailable" }
            val authority = titles.single().substringBefore('/').substringBefore(':')
            check(authority.isNotBlank() && !authority.equals(expected, ignoreCase = true)) { "foreign subscription uses push server" }
            check(ui.performGlobalAction(AccessibilityService.GLOBAL_ACTION_BACK)) { "navigation failed" }
            SystemClock.sleep(500); checked++
        }
        check(checked == 2) { "incomplete comparison" }
        println("WP16_NTFY_FOREIGN_ORIGINS_DISTINCT=true")
    }
}
