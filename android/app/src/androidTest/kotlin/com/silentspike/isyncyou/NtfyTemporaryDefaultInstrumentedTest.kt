package com.silentspike.isyncyou

import android.accessibilityservice.AccessibilityService
import android.os.Bundle
import android.os.SystemClock
import android.system.Os
import android.system.OsConstants
import android.view.accessibility.AccessibilityNodeInfo
import androidx.test.platform.app.InstrumentationRegistry
import java.io.File
import java.io.FileOutputStream
import java.net.URI
import org.json.JSONObject
import org.junit.Test

/** Explicitly selected test-device control, never emits a server address. */
class NtfyTemporaryDefaultInstrumentedTest {
    private val instrumentation = InstrumentationRegistry.getInstrumentation()
    private val automation = instrumentation.uiAutomation
    private val dir = instrumentation.targetContext.filesDir
    private val original = File(dir, "wp16-ntfy-default-display-once.txt")
    private val marker = File(dir, "wp16-ntfy-temporary-default.json")

    private fun nodes(): List<AccessibilityNodeInfo> {
        val root = automation.rootInActiveWindow ?: return emptyList()
        if (root.packageName?.toString() != "io.heckel.ntfy") return emptyList()
        val out = mutableListOf<AccessibilityNodeInfo>()
        fun walk(n: AccessibilityNodeInfo) { out.add(n); for (i in 0 until n.childCount) n.getChild(i)?.let(::walk) }
        walk(root); return out
    }
    private fun setup() { automation.serviceInfo = automation.serviceInfo.apply {
        flags = flags or android.accessibilityservice.AccessibilityServiceInfo.FLAG_REPORT_VIEW_IDS
    } }
    private fun click(node: AccessibilityNodeInfo) {
        var n: AccessibilityNodeInfo? = node
        while (n != null && !n.isClickable) n = n.parent
        check(n?.performAction(AccessibilityNodeInfo.ACTION_CLICK) == true) { "distributor control unavailable" }
    }
    private fun dismissKeyboard() {
        val d = android.os.ParcelFileDescriptor.AutoCloseInputStream(automation.executeShellCommand("dumpsys window"))
            .bufferedReader().use { it.readText() }
        val ime = d.lineSequence().firstOrNull { "type=ime frame=" in it } ?: error("keyboard state unavailable")
        if ("visible=true" in ime) {
            check(automation.performGlobalAction(AccessibilityService.GLOBAL_ACTION_BACK))
            SystemClock.sleep(250)
        }
    }
    private fun openField(): AccessibilityNodeInfo {
        val fieldId = "io.heckel.ntfy:id/default_server_dialog_url"
        var all = nodes()
        all.singleOrNull { it.isVisibleToUser && it.viewIdResourceName == fieldId }?.let { return it }
        val titles = setOf("Default server", "Standardserver", "Standard-Server")
        if (all.none { it.isVisibleToUser && it.text?.toString() in titles + setOf("Notifications", "Benachrichtigungen") }) {
            val more = all.singleOrNull { it.isVisibleToUser && it.isClickable && it.contentDescription?.toString() in setOf("More options", "Weitere Optionen") }
            if (more != null) { click(more); SystemClock.sleep(250) }
            all = nodes()
            click(all.singleOrNull { it.isVisibleToUser && it.text?.toString() in setOf("Settings", "Einstellungen") } ?: error("settings unavailable"))
            SystemClock.sleep(500); all = nodes()
        }
        repeat(6) {
            if (all.any { it.isVisibleToUser && it.text?.toString() in titles }) return@repeat
            val scroll = all.firstOrNull { it.isVisibleToUser && it.isScrollable } ?: return@repeat
            if (!scroll.performAction(AccessibilityNodeInfo.ACTION_SCROLL_FORWARD)) return@repeat
            SystemClock.sleep(250); all = nodes()
        }
        click(all.singleOrNull { it.isVisibleToUser && it.text?.toString() in titles } ?: error("default preference unavailable"))
        SystemClock.sleep(500)
        return nodes().singleOrNull { it.isVisibleToUser && it.viewIdResourceName == fieldId } ?: error("default field unavailable")
    }
    private fun closeWithoutSaving() {
        dismissKeyboard()
        check(automation.performGlobalAction(AccessibilityService.GLOBAL_ACTION_BACK))
        SystemClock.sleep(300)
    }
    private fun setAndSave(field: AccessibilityNodeInfo, value: String) {
        val args = Bundle().apply { putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, value) }
        check(field.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, args)) { "default entry unavailable" }
        dismissKeyboard()
        val save = nodes().singleOrNull { it.isVisibleToUser && it.viewIdResourceName == "io.heckel.ntfy:id/default_server_dialog_action_save" }
            ?: error("default save unavailable")
        click(save); SystemClock.sleep(500)
    }
    private fun privateFile(file: File) {
        val st = Os.lstat(file.path)
        check(OsConstants.S_ISREG(st.st_mode) && st.st_uid == android.os.Process.myUid() &&
            st.st_mode and 0x1ff == 0x180 && st.st_nlink == 1L) { "private fixture invalid" }
    }
    private fun syncDirectory() {
        val fd = Os.open(dir.path, OsConstants.O_RDONLY, 0)
        try { Os.fsync(fd) } finally { Os.close(fd) }
    }
    private fun effectiveOriginal(): String {
        val value = original.readText()
        return if (value.matches(Regex("[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?"))) "https://$value" else value
    }
    private fun writeMarker(value: JSONObject) {
        val data = value.toString().toByteArray()
        val fd = Os.open(marker.path, OsConstants.O_WRONLY or OsConstants.O_CREAT or OsConstants.O_EXCL or OsConstants.O_NOFOLLOW, 0x180)
        try { FileOutputStream(fd).use { it.write(data); it.fd.sync() }; syncDirectory() } finally { data.fill(0) }
    }
    @Test fun setTemporaryDefault() {
        check(BuildConfig.DEBUG) { "debug build required" }; privateFile(original)
        val server = requireNotNull(InstrumentationRegistry.getArguments().getString("temporaryServer"))
        val uri = URI(server)
        check(uri.scheme == "https" && !uri.host.isNullOrBlank() && uri.rawUserInfo == null &&
            (uri.port == -1 || uri.port == 443) && uri.rawQuery == null && uri.rawFragment == null &&
            (uri.rawPath.isNullOrEmpty() || uri.rawPath == "/")) { "invalid server" }
        setup()
        val saved = effectiveOriginal(); val field = openField()
        check(field.text?.toString().orEmpty() == saved) { "default changed" }
        writeMarker(JSONObject().put("version", 1).put("temporary", server))
        setAndSave(field, server)
        val checkField = openField(); check(checkField.text?.toString().orEmpty() == server) { "temporary default unconfirmed" }
        closeWithoutSaving()
        println("WP16_NTFY_TEMP_DEFAULT=confirmed")
    }
    @Test fun restoreOriginalDefault() {
        check(BuildConfig.DEBUG) { "debug build required" }; privateFile(original); privateFile(marker)
        val target = JSONObject(marker.readText()).getString("temporary")
        val saved = effectiveOriginal()
        val uri = URI(saved)
        check(uri.scheme == "https" && !uri.host.isNullOrBlank() && uri.rawUserInfo == null && uri.rawQuery == null && uri.rawFragment == null) { "saved default invalid" }
        setup()
        val field = openField(); val current = field.text?.toString().orEmpty()
        check(current == target || current == saved) { "default changed externally" }
        if (current == target && current != saved) setAndSave(field, saved) else closeWithoutSaving()
        val checkField = openField(); check(checkField.text?.toString().orEmpty() == saved) { "default restore unconfirmed" }
        closeWithoutSaving()
        check(marker.delete()) { "restore marker cleanup failed" }; syncDirectory()
        println("WP16_NTFY_DEFAULT_RESTORED=true")
    }
}
