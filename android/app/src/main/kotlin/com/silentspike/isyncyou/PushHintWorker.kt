package com.silentspike.isyncyou

import android.content.Context
import androidx.work.CoroutineWorker
import androidx.work.WorkerParameters
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import org.json.JSONObject

/** Only reconcile an authenticated pending hint; enrollment has its own work chain. */
class PushHintWorker(context: Context, params: WorkerParameters) : CoroutineWorker(context, params) {
    override suspend fun doWork(): Result = withContext(Dispatchers.IO) {
        val instance = inputData.getString("instance") ?: return@withContext Result.success()
        if (!instance.matches(Regex("[A-Za-z0-9_-]{16,128}"))) return@withContext Result.success()
        try {
            val state = PushEnrollmentStore.get(applicationContext, instance) ?: return@withContext Result.success()
            val account = state.optString("account", "")
            val origin = state.optString("origin", "")
            if (!state.optBoolean("hint_pending", false) || !PushHintPolicy.authorized(state, account, origin)) return@withContext Result.success()
            val identity = PushInstallationKey.existing(applicationContext, account) ?: return@withContext Result.success()
            if (identity.installation != instance) return@withContext Result.success()
            if (EngineBootstrap.ensureStarted(applicationContext.filesDir).isEmpty()) return@withContext Result.retry()
            if (NativeEngine.nativeProviderOrigin(applicationContext.filesDir.absolutePath, account) != origin) return@withContext Result.success()
            val generation = state.optLong("hint_generation", 0)
            if (NativeEngine.nativeWaitProviderRefresh(account) != 1) return@withContext Result.retry()
            if (PushInstallationKey.existing(applicationContext, account)?.installation != instance) return@withContext Result.success()
            PushEnrollmentStore.update(applicationContext, instance) {
                if (!PushHintPolicy.mayClear(it, account, origin, generation)) return@update false
                it.put("hint_pending", false).put("last_sync_at", System.currentTimeMillis() / 1000)
                true
            }
            Result.success()
        } catch (_: Exception) {
            Result.retry()
        }
    }
}

internal object PushHintPolicy {
    fun authorized(state: JSONObject, account: String, origin: String): Boolean {
        val stage = state.optString("stage")
        return account.matches(Regex("[A-Za-z0-9._-]{1,128}")) && origin.isNotEmpty() &&
            state.optString("account") == account && state.optString("origin") == origin &&
            !stage.endsWith("_logout") && stage !in setOf("destroy_unknown", "revoke_unknown")
    }
    fun mayClear(state: JSONObject, account: String, origin: String, generation: Long): Boolean =
        authorized(state, account, origin) && state.optLong("hint_generation", 0) == generation
}
