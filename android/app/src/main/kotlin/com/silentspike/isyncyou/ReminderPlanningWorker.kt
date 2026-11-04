package com.silentspike.isyncyou

import android.content.Context
import androidx.work.Constraints
import androidx.work.CoroutineWorker
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.ExistingWorkPolicy
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import androidx.work.workDataOf
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import java.util.TimeZone
import java.util.concurrent.TimeUnit
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import org.json.JSONObject

internal data class ReminderAccountOutcome(val successful: Boolean, val unsupported: Boolean = false, val reason: String = "")
internal data class ReminderBatchOutcome(val status: String, val retry: Boolean, val reason: String)

internal object ReminderBatchPolicy {
    fun run(accounts: List<String>, execute: (String) -> ReminderAccountOutcome): ReminderBatchOutcome {
        if (accounts.isEmpty()) return ReminderBatchOutcome("idle", false, "")
        val outcomes = accounts.map { account ->
            try { execute(account) } catch (e: kotlinx.coroutines.CancellationException) { throw e }
            catch (_: Exception) { ReminderAccountOutcome(false) }
        }
        val failed = outcomes.any { !it.successful }
        val partial = outcomes.any { it.unsupported } || (failed && outcomes.any { it.successful })
        return ReminderBatchOutcome(if (partial) "partial_coverage" else if (failed) "unavailable" else "ready", failed,
            outcomes.firstOrNull { !it.successful }?.reason ?: "")
    }
}

/** Reconciles bounded native plans without persisting calendar content in WorkManager. */
class ReminderPlanningWorker(context: Context, params: WorkerParameters) : CoroutineWorker(context, params) {
    override suspend fun doWork(): Result = withContext(Dispatchers.IO) {
        ReminderStatus.record(applicationContext, "planning")
        try {
            if (EngineBootstrap.ensureStarted(applicationContext.filesDir).isEmpty()) {
                ReminderStatus.record(applicationContext, "unavailable"); return@withContext Result.retry()
            }
            val base = applicationContext.filesDir.absolutePath
            val accounts = JSONObject(NativeEngine.nativeConnectedProviderAccounts(base))
            if (!accounts.getBoolean("ok")) {
                ReminderStatus.record(applicationContext, "unavailable"); return@withContext Result.retry()
            }
            val now = System.currentTimeMillis() / 1000
            val format = SimpleDateFormat("yyyyMMdd'T'HHmmss'Z'", Locale.US).apply { timeZone = TimeZone.getTimeZone("UTC") }
            val wm = WorkManager.getInstance(applicationContext)
            val ids = accounts.getJSONArray("accounts")
            val batch = ReminderBatchPolicy.run((0 until ids.length()).map(ids::getString)) { account ->
                val request = JSONObject().put("account", account)
                    .put("occurrence_start", format.format(Date((now - 369L * 86400) * 1000)))
                    .put("occurrence_end", format.format(Date((now + 369L * 86400) * 1000)))
                    .put("window_start", now - 86400).put("window_end", now + 86400)
                    .put("timezone", TimeZone.getDefault().id)
                val result = JSONObject(NativeEngine.nativeReminderPlan(base, request.toString()))
                if (!result.optBoolean("ok")) {
                    val reason = result.optString("reason").takeIf { it in setOf("reminder_window", "reminder_changed", "reminder_config", "reminder_client", "reminder_discovery", "reminder_sync", "reminder_cache", "reminder_events_report", "reminder_tasks_report", "reminder_read", "reminder_access_check", "reminder_access_denied", "reminder_resource_get", "reminder_plan") } ?: ""
                    ReminderAccountOutcome(false, reason = reason)
                } else {
                  val unsupported = result.getInt("unsupported_count")
                  require(unsupported >= 0)
                  val records = result.getJSONArray("records")
                  for (j in 0 until records.length()) {
                    val record = records.getJSONObject(j)
                    val tag = record.getString("notification_tag")
                    require(tag.matches(Regex("[0-9a-f]{64}")))
                    val trigger = record.getLong("trigger_at")
                    require(trigger in (now - 86400)..(now + 86400))
                    PushEnrollmentStore.put(applicationContext, "r_$tag", record)
                    val work = OneTimeWorkRequestBuilder<ReminderWorker>()
                        .setInputData(workDataOf("tag" to tag))
                        .setConstraints(Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build())
                        .setInitialDelay((trigger - now).coerceAtLeast(0), TimeUnit.SECONDS).build()
                    wm.enqueueUniqueWork("calendar-reminder:$tag", ExistingWorkPolicy.KEEP, work)
                  }
                  ReminderAccountOutcome(true, unsupported > 0)
                }
            }
            ReminderStatus.record(applicationContext, batch.status, batch.reason)
            if (batch.retry) Result.retry() else Result.success()
        } catch (e: kotlinx.coroutines.CancellationException) {
            throw e
        } catch (_: Exception) {
            ReminderStatus.record(applicationContext, "unavailable")
            Result.retry()
        }
    }

    companion object {
        private const val NAME = "calendar-reminder-planning"
        fun schedule(context: Context) {
            val work = PeriodicWorkRequestBuilder<ReminderPlanningWorker>(15, TimeUnit.MINUTES)
                .setConstraints(Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build()).build()
            WorkManager.getInstance(context).enqueueUniquePeriodicWork(NAME, ExistingPeriodicWorkPolicy.KEEP, work)
        }
    }
}
