package com.silentspike.isyncyou

import android.Manifest
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.os.BatteryManager
import android.os.Build
import android.os.StatFs
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat
import androidx.work.CoroutineWorker
import androidx.work.ForegroundInfo
import androidx.work.WorkerParameters
import androidx.work.WorkManager
import androidx.work.Data
import androidx.work.Constraints
import androidx.work.NetworkType
import androidx.work.ExistingWorkPolicy
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.BackoffPolicy
import androidx.work.workDataOf
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import org.json.JSONObject
import java.util.concurrent.TimeUnit

internal enum class MobileJobKindWire(val wire: String) {
    BACKUP("backup"),
    RESTORE_CLOUD("restore-cloud");

    companion object {
        fun parse(value: String?): MobileJobKindWire? = entries.firstOrNull { it.wire == value }
    }
}

internal data class MobileJobInput(val jobId: String, val kind: MobileJobKindWire)

internal data class MobileJobConstraintsWire(
    val wifiOnly: Boolean,
    val chargingOnly: Boolean,
    val minFreeBytes: Long,
)

internal data class MobileJobPlan(
    val jobs: List<MobileJobInput>,
    val constraints: MobileJobConstraintsWire,
)

internal data class MobileDeviceSnapshot(
    val networkValidated: Boolean,
    val metered: Boolean,
    val charging: Boolean,
    val freeBytes: Long,
)

internal data class MobileJobResponse(val status: String, val code: String?)

internal data class MobileJobControllerResult(
    val result: MobileJobWorkerPolicy.WorkerResult,
    val code: String?,
)

internal object MobileJobWorkerPolicy {
    const val JOB_ID = "job_id"
    const val KIND = "kind"
    const val MAX_JOB_ID_LENGTH = 128
    const val CHANNEL_ID = "mobile_jobs"
    const val NOTIFICATION_ID = 62001

    fun parseInput(data: Data): MobileJobInput? {
        if (data.keyValueMap.keys.any { it != JOB_ID && it != KIND }) return null
        val jobId = data.getString(JOB_ID) ?: return null
        if (jobId.length !in 1..MAX_JOB_ID_LENGTH ||
            !jobId.all { it in 'a'..'z' || it in 'A'..'Z' || it in '0'..'9' || it == '.' || it == '_' || it == '-' }
        ) return null
        val kind = MobileJobKindWire.parse(data.getString(KIND)) ?: return null
        return MobileJobInput(jobId, kind)
    }

    fun parseResponse(raw: String): MobileJobResponse? = runCatching {
        val json = JSONObject(raw)
        if (json.optInt("v", -1) != 1) return null
        val status = json.optString("status", "")
        if (status !in setOf("succeeded", "retry", "failed")) return null
        val code = json.optString("code", "").takeIf { it.isNotEmpty() }
        MobileJobResponse(status, code)
    }.getOrNull()

    fun resultFor(response: MobileJobResponse): WorkerResult {
        return when (response.status) {
            "succeeded" -> WorkerResult.Success
            "retry" -> WorkerResult.Retry
            else -> if (response.code == "notifications_required") {
                WorkerResult.Failure
            } else {
                WorkerResult.Failure
            }
        }
    }

    enum class WorkerResult { Success, Retry, Failure }
}

internal object MobileJobSchedulerPolicy {
    const val MAX_JOBS = 64

    fun parsePlan(raw: String): MobileJobPlan? = runCatching {
        val plan = JSONObject(raw)
        if (plan.optInt("v", -1) != 1 || plan.optString("status") != "ok") return null
        val constraintsJson = plan.optJSONObject("constraints") ?: return null
        val wifiOnly = constraintsJson.opt("wifi_only") as? Boolean ?: return null
        val chargingOnly = constraintsJson.opt("charging_only") as? Boolean ?: return null
        val minFree = (constraintsJson.opt("min_free_bytes") as? Number)?.toLong() ?: return null
        if (minFree < 0) return null
        val jobsJson = plan.optJSONArray("jobs") ?: return null
        if (jobsJson.length() > MAX_JOBS) return null
        val jobs = ArrayList<MobileJobInput>(jobsJson.length())
        for (index in 0 until jobsJson.length()) {
            val job = jobsJson.optJSONObject(index) ?: return null
            val input = Data.Builder()
                .putString(MobileJobWorkerPolicy.JOB_ID, job.optString("job_id", ""))
                .putString(MobileJobWorkerPolicy.KIND, job.optString("kind", ""))
                .build()
            jobs += MobileJobWorkerPolicy.parseInput(input) ?: return null
        }
        MobileJobPlan(jobs, MobileJobConstraintsWire(wifiOnly, chargingOnly, minFree))
    }.getOrNull()

    fun workConstraints(policy: MobileJobConstraintsWire): Constraints = Constraints.Builder()
        .setRequiredNetworkType(if (policy.wifiOnly) NetworkType.UNMETERED else NetworkType.CONNECTED)
        .setRequiresCharging(policy.chargingOnly)
        .setRequiresBatteryNotLow(true)
        .setRequiresStorageNotLow(true)
        .build()
}

internal object MobileJobNotificationPolicy {
    fun canPublish(
        apiLevel: Int,
        notificationsEnabled: Boolean,
        runtimePermissionGranted: Boolean,
        channelImportance: Int?,
    ): Boolean {
        if (!notificationsEnabled) return false
        if (apiLevel >= 33 && !runtimePermissionGranted) return false
        return apiLevel < 26 || channelImportance != null &&
            channelImportance != NotificationManager.IMPORTANCE_NONE
    }
}

internal interface MobileJobForegroundController {
    suspend fun publish(job: MobileJobInput): Boolean
}

internal interface MobileJobNativeController {
    fun start(filesDir: java.io.File): Boolean
    fun run(input: MobileJobInput, device: MobileDeviceSnapshot): String
}

internal interface MobileDeviceStateProvider {
    fun snapshot(): MobileDeviceSnapshot?
}

internal class AndroidMobileDeviceStateProvider(private val context: Context) : MobileDeviceStateProvider {
    override fun snapshot(): MobileDeviceSnapshot? = runCatching {
        val connectivity = context.getSystemService(ConnectivityManager::class.java)
            ?: return null
        val network = connectivity.activeNetwork ?: return null
        val caps = connectivity.getNetworkCapabilities(network) ?: return null
        val validated = caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_VALIDATED)
        val metered = connectivity.isActiveNetworkMetered
        val battery = context.getSystemService(BatteryManager::class.java)
            ?: return null
        val charging = battery.isCharging
        val stat = StatFs(context.filesDir.absolutePath)
        val freeBytes = stat.availableBytes
        if (freeBytes < 0) return null
        MobileDeviceSnapshot(validated, metered, charging, freeBytes)
    }.getOrNull()
}

internal class AndroidMobileJobForegroundController(
    private val context: Context,
    private val setForeground: suspend (ForegroundInfo) -> Unit,
) : MobileJobForegroundController {
    override suspend fun publish(job: MobileJobInput): Boolean {
        val manager = context.getSystemService(NotificationManager::class.java) ?: return false
        ensureChannel(manager)
        val permissionGranted = ContextCompat.checkSelfPermission(
            context,
            Manifest.permission.POST_NOTIFICATIONS,
        ) == PackageManager.PERMISSION_GRANTED
        val channelImportance = if (Build.VERSION.SDK_INT >= 26) {
            manager.getNotificationChannel(MobileJobWorkerPolicy.CHANNEL_ID)?.importance
        } else {
            null
        }
        if (!MobileJobNotificationPolicy.canPublish(
                Build.VERSION.SDK_INT,
                NotificationManagerCompat.from(context).areNotificationsEnabled(),
                permissionGranted,
                channelImportance,
            )
        ) return false
        val notification: Notification = NotificationCompat.Builder(
            context,
            MobileJobWorkerPolicy.CHANNEL_ID,
        )
            .setSmallIcon(com.silentspike.isyncyou.R.drawable.ic_stat_isyncyou)
            .setContentTitle(context.getString(com.silentspike.isyncyou.R.string.app_name))
            .setContentText("${job.kind.wire} in progress")
            .setOngoing(true)
            .setCategory(NotificationCompat.CATEGORY_PROGRESS)
            .setProgress(0, 0, true)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .build()
        return runCatching {
            setForeground(
                ForegroundInfo(
                    MobileJobWorkerPolicy.NOTIFICATION_ID,
                    notification,
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC,
                ),
            )
            true
        }.getOrDefault(false)
    }

    private fun ensureChannel(manager: NotificationManager) {
        if (Build.VERSION.SDK_INT >= 26 &&
            manager.getNotificationChannel(MobileJobWorkerPolicy.CHANNEL_ID) == null
        ) {
            manager.createNotificationChannel(
                NotificationChannel(
                    MobileJobWorkerPolicy.CHANNEL_ID,
                    "Mobile jobs",
                    NotificationManager.IMPORTANCE_LOW,
                ).apply { description = "Visible progress for mobile cloud jobs" },
            )
        }
    }
}

internal class MobileJobWorkerController(
    private val foreground: MobileJobForegroundController,
    private val deviceState: MobileDeviceStateProvider,
    private val native: MobileJobNativeController,
    private val filesDir: java.io.File,
) {
    suspend fun run(input: MobileJobInput): MobileJobControllerResult {
        // This ordering is a security invariant: no engine, lease, or Graph work can
        // happen before the user-visible foreground notification is accepted.
        if (!foreground.publish(input)) {
            return MobileJobControllerResult(MobileJobWorkerPolicy.WorkerResult.Failure, "notifications_required")
        }
        val snapshot = deviceState.snapshot()
            ?: return MobileJobControllerResult(MobileJobWorkerPolicy.WorkerResult.Retry, "device_state_unavailable")
        if (!snapshot.networkValidated) {
            return MobileJobControllerResult(MobileJobWorkerPolicy.WorkerResult.Retry, "network_unvalidated")
        }
        if (!native.start(filesDir)) {
            return MobileJobControllerResult(MobileJobWorkerPolicy.WorkerResult.Retry, "engine_start_failed")
        }
        val response = MobileJobWorkerPolicy.parseResponse(native.run(input, snapshot))
            ?: return MobileJobControllerResult(MobileJobWorkerPolicy.WorkerResult.Failure, "invalid_native_response")
        return MobileJobControllerResult(MobileJobWorkerPolicy.resultFor(response), response.code)
    }
}

class MobileJobWorker(appContext: Context, params: WorkerParameters) : CoroutineWorker(appContext, params) {
    override suspend fun doWork(): Result = withContext(Dispatchers.IO) {
        val input = MobileJobWorkerPolicy.parseInput(inputData)
            ?: return@withContext Result.failure(workDataOf("code" to "invalid_input"))
        val controller = MobileJobWorkerController(
            foreground = AndroidMobileJobForegroundController(applicationContext, ::setForeground),
            deviceState = AndroidMobileDeviceStateProvider(applicationContext),
            native = object : MobileJobNativeController {
                override fun start(filesDir: java.io.File): Boolean =
                    EngineBootstrap.ensureStarted(filesDir).isNotEmpty()

                override fun run(input: MobileJobInput, device: MobileDeviceSnapshot): String {
                    val request = JSONObject()
                        .put("v", 1)
                        .put("job_id", input.jobId)
                        .put("kind", input.kind.wire)
                        .put("device", JSONObject()
                            .put("network_validated", device.networkValidated)
                            .put("metered", device.metered)
                            .put("charging", device.charging)
                            .put("free_bytes", device.freeBytes))
                    return NativeEngine.nativeRunMobileJob(request.toString())
                }
            },
            filesDir = applicationContext.filesDir,
        )
        val outcome = controller.run(input)
        when (outcome.result) {
            MobileJobWorkerPolicy.WorkerResult.Success -> Result.success()
            MobileJobWorkerPolicy.WorkerResult.Retry -> Result.retry()
            MobileJobWorkerPolicy.WorkerResult.Failure ->
                Result.failure(workDataOf("code" to (outcome.code ?: "mobile_job_failed")))
        }
    }

}

object MobileJobScheduler {
    private fun enqueue(
        context: Context,
        job: MobileJobInput,
        policy: MobileJobConstraintsWire,
    ): Boolean {
        val input = Data.Builder()
            .putString(MobileJobWorkerPolicy.JOB_ID, job.jobId)
            .putString(MobileJobWorkerPolicy.KIND, job.kind.wire)
            .build()
        if (MobileJobWorkerPolicy.parseInput(input) == null) return false
        val request = OneTimeWorkRequestBuilder<MobileJobWorker>()
            .setInputData(input)
            .setConstraints(MobileJobSchedulerPolicy.workConstraints(policy))
            .setBackoffCriteria(BackoffPolicy.EXPONENTIAL, 30, TimeUnit.SECONDS)
            .build()
        WorkManager.getInstance(context).enqueueUniqueWork(
            "mobile-job:${job.jobId}",
            ExistingWorkPolicy.KEEP,
            request,
        )
        return true
    }

    /** Reconcile Rust's durable plan after an exact successful queue-producing POST. */
    fun reconcile(context: Context) {
        val plan = MobileJobSchedulerPolicy.parsePlan(NativeEngine.nativeMobileJobPlan()) ?: return
        for (job in plan.jobs) {
            enqueue(context, job, plan.constraints)
        }
    }
}
