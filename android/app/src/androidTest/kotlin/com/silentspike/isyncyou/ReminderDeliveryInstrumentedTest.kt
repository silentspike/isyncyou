package com.silentspike.isyncyou

import android.app.NotificationManager
import android.os.SystemClock
import androidx.test.platform.app.InstrumentationRegistry
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.WorkInfo
import androidx.work.WorkManager
import java.io.File
import java.util.concurrent.TimeUnit
import org.json.JSONObject
import org.junit.Test

/** One controlled fixture, using the production planner and notification worker. */
class ReminderDeliveryInstrumentedTest {
    @Test fun plannedFixtureReachesOsAndIsAcknowledged() {
        check(BuildConfig.DEBUG)
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val fixture = JSONObject(File(context.filesDir, "wp16-reminder-fixture.json").readText())
        val account = fixture.getString("account")
        val uid = fixture.getString("uid")
        check(account.isNotEmpty() && uid.isNotEmpty())
        check(EngineBootstrap.ensureStarted(context.filesDir).isNotEmpty())
        val manager = context.getSystemService(NotificationManager::class.java)
        val workManager = WorkManager.getInstance(context)
        val initial = manager.activeNotifications.mapNotNull { it.tag }.toSet()
        val request = OneTimeWorkRequestBuilder<ReminderPlanningWorker>().build()
        val started = SystemClock.elapsedRealtime()
        workManager.enqueue(request).result.get(30, TimeUnit.SECONDS)
        val planDeadline = started + TimeUnit.MINUTES.toMillis(5)
        var planState: WorkInfo.State? = null
        while (SystemClock.elapsedRealtime() < planDeadline) {
            val info = workManager.getWorkInfoById(request.id).get(10, TimeUnit.SECONDS)
            planState = info?.state
            if (planState?.isFinished == true || (planState == WorkInfo.State.ENQUEUED && (info?.runAttemptCount ?: 0) > 0)) break
            SystemClock.sleep(1000)
        }
        check(planState == WorkInfo.State.SUCCEEDED) { "fixture plan unavailable" }
        println("WP16_REMINDER_PLAN_MS=${SystemClock.elapsedRealtime() - started}")
        val deadline = started + TimeUnit.MINUTES.toMillis(8)
        var acknowledged = false
        val selected = mutableMapOf<String, JSONObject>()
        while (SystemClock.elapsedRealtime() < deadline && !acknowledged) {
            for (key in context.getSharedPreferences("push_enrollment_v1", android.content.Context.MODE_PRIVATE).all.keys) {
                if (!key.matches(Regex("r_[0-9a-f]{64}"))) continue
                val record = runCatching { PushEnrollmentStore.get(context, key) }.getOrNull() ?: continue
                if (record.optString("account") == account && record.optString("uid") == uid) selected[key.removePrefix("r_")] = record
            }
            for (notification in manager.activeNotifications) {
                val tag = notification.tag ?: continue
                if (tag in initial || !tag.matches(Regex("[0-9a-f]{64}"))) continue
                val record = selected[tag] ?: continue
                val claim = JSONObject(NativeEngine.nativeReminderClaim(context.filesDir.absolutePath, record.toString()))
                if (claim.optString("status") == "delivered") { acknowledged = true; break }
            }
            if (!acknowledged) SystemClock.sleep(1000)
        }
        check(acknowledged) { "fixture notification and acknowledgement unavailable" }
        println("WP16_REMINDER_OS_AND_ACK=true")
    }
}
