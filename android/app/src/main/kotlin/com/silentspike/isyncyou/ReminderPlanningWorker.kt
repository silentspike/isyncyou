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

internal data class ReminderAccountOutcome(val successful: Boolean, val unsupported: Boolean = false, val reason: String = "", val retry: Boolean = false)
internal data class ReminderBatchOutcome(val status: String, val retry: Boolean, val reason: String)

internal object ReminderBatchPolicy {
    fun run(accounts: List<String>, execute: (String) -> ReminderAccountOutcome): ReminderBatchOutcome {
        if (accounts.isEmpty()) return ReminderBatchOutcome("idle", false, "")
        val outcomes = accounts.map { account ->
            try { execute(account) } catch (e: kotlinx.coroutines.CancellationException) { throw e }
            catch (_: Exception) { ReminderAccountOutcome(false) }
        }
        val failed = outcomes.any { !it.successful || it.retry }
        val partial = outcomes.any { it.unsupported || (it.retry && it.successful) } || (failed && outcomes.any { it.successful })
        return ReminderBatchOutcome(if (partial) "partial_coverage" else if (failed) "unavailable" else "ready", failed,
            outcomes.firstOrNull { !it.successful || it.retry }?.reason ?: "")
    }
}

internal fun readReminderRecords(keys: List<String>, read: (String) -> JSONObject?): Pair<List<Pair<String, JSONObject>>, Boolean> {
    var unreadable = false
    val records = mutableListOf<Pair<String, JSONObject>>()
    for (key in keys) {
        val record = try { read(key) } catch (_: Exception) { unreadable = true; continue }
        if (record != null) records.add(key to record)
    }
    return records to unreadable
}

internal fun invalidReminderScope(record: JSONObject, key: String, account: String, completed: Set<String>): Boolean {
    val fields = setOf("account", "collection", "href", "occurrence_start", "occurrence_end", "window_start", "window_end", "floating_tzid", "uid", "recurrence_id", "alarm_id", "trigger_at", "notification_tag")
    if (record.keys().asSequence().toSet() != fields) return false
    if (record.optString("notification_tag") != key.removePrefix("r_")) return false
    if (record.optString("account") != account) return false
    val collection = record.optString("collection")
    return collection.isNotEmpty() && collection !in completed
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
                val ok = result.optBoolean("ok")
                val completed = if (ok) result.getJSONArray("completed_collections").let { values ->
                    (0 until values.length()).map(values::getString).toSet()
                } else emptySet()
                // Remove obsolete records before cancelling their work. A worker already
                // running still rechecks live access and identity before notification.
                val (oldRecords, unreadableRecord) = readReminderRecords(
                    PushEnrollmentStore.reminderKeys(applicationContext)
                ) { key -> PushEnrollmentStore.get(applicationContext, key) }
                for ((key, old) in oldRecords) {
                    if (!invalidReminderScope(old, key, account, completed)) continue
                    if (PushEnrollmentStore.removeIf(applicationContext, key) { current ->
                            invalidReminderScope(current, key, account, completed)
                        }) {
                        wm.cancelUniqueWork("calendar-reminder:${key.removePrefix("r_")}").result.get()
                    }
                }
                if (!ok) {
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
                    wm.enqueueUniqueWork("calendar-reminder:$tag", ExistingWorkPolicy.KEEP, work).result.get()
                  }
                  val failed = result.getInt("failed_collections")
                  require(failed >= 0)
                  val reason = result.optString("failure_reason").takeIf { it in setOf("reminder_window", "reminder_changed", "reminder_config", "reminder_client", "reminder_discovery", "reminder_sync", "reminder_cache", "reminder_events_report", "reminder_tasks_report", "reminder_read", "reminder_access_check", "reminder_access_denied", "reminder_resource_get", "reminder_plan") } ?: "reminder_plan"
                  val retry = failed > 0 || unreadableRecord
                  ReminderAccountOutcome(completed.isNotEmpty() || failed == 0, unsupported > 0,
                      if (failed > 0) reason else if (unreadableRecord) "reminder_plan" else "", retry)
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
