package com.silentspike.isyncyou

import android.content.Context
import androidx.work.Constraints
import androidx.work.CoroutineWorker
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.NetworkType
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import org.json.JSONObject
import java.util.concurrent.TimeUnit

/** Account reconciliation also runs when no UnifiedPush distributor is configured. */
class ProviderCatchUpWorker(context: Context, params: WorkerParameters) : CoroutineWorker(context, params) {
    override suspend fun doWork(): Result = withContext(Dispatchers.IO) {
        try {
            if (EngineBootstrap.ensureStarted(applicationContext.filesDir).isEmpty()) return@withContext Result.retry()
            val response = JSONObject(NativeEngine.nativeConnectedProviderAccounts(applicationContext.filesDir.absolutePath))
            if (!response.getBoolean("ok")) return@withContext Result.retry()
            val accounts = response.getJSONArray("accounts")
            if (accounts.length() == 0) return@withContext Result.success()
            val pass = JSONObject(NativeEngine.nativeWaitProviderRefreshResults())
            if (!pass.getBoolean("ok")) return@withContext Result.retry()
            val outcomes = pass.getJSONObject("accounts")
            val requested = mutableListOf<String>()
            val completed = mutableMapOf<String, Boolean>()
            for (i in 0 until accounts.length()) {
                val account = accounts.getString(i)
                requested.add(account)
                completed[account] = outcomes.optBoolean(account, false)
                // Resolve push enrollment and renewal independently of this delta pass.
                runCatching { PushInstallationKey.existing(applicationContext, account) }.getOrNull()?.let {
                    WorkManager.getInstance(applicationContext).cancelUniqueWork("provider-push-catch-up:${it.installation}")
                    PushScheduler.enqueue(applicationContext, it.installation, queueAfter = false)
                }
            }
            if (ProviderCatchUpPolicy.complete(requested, completed)) Result.success() else Result.retry()
        } catch (_: Exception) {
            Result.retry()
        }
    }

    companion object {
        private const val NAME = "provider-account-catch-up"
        fun schedule(context: Context) {
            val request = PeriodicWorkRequestBuilder<ProviderCatchUpWorker>(15, TimeUnit.MINUTES)
                .setConstraints(Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build())
                .setInitialDelay(15, TimeUnit.MINUTES)
                .build()
            WorkManager.getInstance(context).enqueueUniquePeriodicWork(NAME, ExistingPeriodicWorkPolicy.KEEP, request)
        }
        fun cancel(context: Context) {
            WorkManager.getInstance(context).cancelUniqueWork(NAME)
        }
    }
}
