package com.silentspike.isyncyou

import android.content.Context
import androidx.work.CoroutineWorker
import androidx.work.WorkerParameters
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

/** Durable, bounded broker enrollment and catch-up; no separate persistent connection. */
class PushEnrollmentWorker(context: Context, params: WorkerParameters) : CoroutineWorker(context, params) {
    override suspend fun doWork(): Result = withContext(Dispatchers.IO) {
        val instance = inputData.getString("instance") ?: return@withContext Result.failure()
        if (!instance.matches(Regex("[A-Za-z0-9_-]{16,128}"))) return@withContext Result.failure()
        val state = runCatching { PushEnrollmentStore.get(applicationContext, instance) }.getOrNull()
            ?: return@withContext Result.failure()
        val account = state.optString("account", "")
        val endpoint = state.optString("endpoint", "")
        if (!account.matches(Regex("[A-Za-z0-9._-]{1,128}"))) return@withContext Result.failure()
        if (!runCatching { EngineBootstrap.ensureStarted(applicationContext.filesDir).isNotEmpty() }.getOrDefault(false)) {
            return@withContext Result.retry()
        }
        try {
            when (state.optString("stage")) {
                "endpoint_received" -> {
                    // Record intent before the remote mutation. A lost reply is never resent blindly.
                    if (!PushEnrollmentStore.update(applicationContext, instance) {
                            if (it.optString("stage") != "endpoint_received") return@update false
                            it.put("stage", "prepare_unknown"); true
                        }) return@withContext Result.success()
                    val result = PushBroker.operation(applicationContext, account, endpoint, "prepare")
                    PushEnrollmentStore.update(applicationContext, instance) {
                        if (it.optString("stage") != "prepare_unknown") return@update false
                        it.put("username", result.getString("username"))
                            .put("password", result.getString("password"))
                            .put("expires", result.getLong("expires")).put("stage", "credentials_ready")
                        true
                    }
                }
                "proof_received" -> {
                    val nonce = state.getString("proof_nonce")
                    if (!PushEnrollmentStore.update(applicationContext, instance) {
                            if (it.optString("stage") != "proof_received") return@update false
                            it.put("stage", "confirm_unknown"); true
                        }) return@withContext Result.success()
                    val result = PushBroker.operation(applicationContext, account, endpoint, "confirm", nonce)
                    check(result.optString("status") == "active")
                    PushEnrollmentStore.update(applicationContext, instance) {
                        if (it.optString("stage") != "confirm_unknown") return@update false
                        it.remove("password"); it.remove("proof_nonce"); it.put("stage", "broker_bound"); true
                    }
                }
            }
            val current = PushEnrollmentStore.get(applicationContext, instance) ?: return@withContext Result.failure()
            if (current.optString("stage") == "broker_bound") {
                val metadata = PushJmap.call(applicationContext, account, "metadata")
                val oldVapid = if (current.isNull("vapid")) null else current.optString("vapid")
                val newVapid = if (metadata.isNull("vapid")) null else metadata.optString("vapid")
                check(oldVapid == newVapid && current.optString("session_state") == metadata.optString("session_state"))
                val create = org.json.JSONObject().put("creation_id", "push")
                    .put("device_client_id", instance).put("endpoint", endpoint)
                    .put("p256dh", current.getString("public_key")).put("auth", current.getString("auth"))
                if (!PushEnrollmentStore.update(applicationContext, instance) {
                        if (it.optString("stage") != "broker_bound") return@update false
                        it.put("stage", "create_unknown"); true
                    }) return@withContext Result.success()
                val outcome = PushJmap.call(applicationContext, account, "create", create)
                check(outcome.optString("status") == "applied")
                val id = outcome.getString("id")
                val expiry = outcome.optString("expires", "").takeIf { PushJmap.expiryMillis(it) != null }
                    ?: PushJmap.find(applicationContext, account, instance, endpoint,
                        current.getString("public_key"), current.getString("auth"))?.optString("expires", "")
                require(expiry != null && PushJmap.expiryMillis(expiry) != null)
                PushEnrollmentStore.update(applicationContext, instance) {
                    if (it.optString("stage") != "create_unknown") return@update false
                    it.put("subscription_id", id).put("expires", expiry)
                        .put("stage", "verification_pending"); true
                }
            }
            val afterCreate = PushEnrollmentStore.get(applicationContext, instance) ?: return@withContext Result.failure()
            if (afterCreate.optString("stage") == "create_unknown") {
                // A read can reconcile a lost response; absence is not proof to retry create.
                val metadata = PushJmap.call(applicationContext, account, "metadata")
                val oldVapid = if (afterCreate.isNull("vapid")) null else afterCreate.optString("vapid")
                val newVapid = if (metadata.isNull("vapid")) null else metadata.optString("vapid")
                if (oldVapid != newVapid) {
                    PushEnrollmentStore.update(applicationContext, instance) { it.put("stage", "vapid_changed"); true }
                    return@withContext Result.failure()
                }
                val existing = PushJmap.find(applicationContext, account, instance, endpoint,
                    afterCreate.getString("public_key"), afterCreate.getString("auth"))
                if (existing != null && existing.optString("id").isNotEmpty() &&
                    PushJmap.expiryMillis(existing.optString("expires", "")) != null) {
                    PushEnrollmentStore.update(applicationContext, instance) {
                        if (it.optString("stage") != "create_unknown") return@update false
                        it.put("subscription_id", existing.getString("id"))
                            .put("expires", existing.opt("expires")).put("stage", "verification_pending"); true
                    }
                }
            }
            val pending = PushEnrollmentStore.get(applicationContext, instance) ?: return@withContext Result.failure()
            if (pending.optString("stage") == "verification_pending" &&
                pending.optString("verify_id") == pending.optString("subscription_id") &&
                pending.optString("verify_code").isNotEmpty()) {
                val id = pending.getString("subscription_id")
                val code = pending.getString("verify_code")
                if (!PushEnrollmentStore.update(applicationContext, instance) {
                        if (it.optString("stage") != "verification_pending") return@update false
                        it.put("stage", "verify_unknown"); true
                    }) return@withContext Result.success()
                val result = PushJmap.call(applicationContext, account, "verify", org.json.JSONObject().put("id", id).put("code", code))
                check(result.optString("status") == "applied")
                PushEnrollmentStore.update(applicationContext, instance) {
                    if (it.optString("stage") != "verify_unknown") return@update false
                    it.remove("verify_code"); it.remove("verify_id"); it.put("stage", "active"); true
                }
                val active = PushEnrollmentStore.get(applicationContext, instance)
                active?.optString("expires")?.takeIf { it.isNotEmpty() }?.let {
                    PushJmap.scheduleRenewal(applicationContext, instance, it)
                }
            }
            val active = PushEnrollmentStore.get(applicationContext, instance) ?: return@withContext Result.failure()
            val expires = active.optString("expires", "")
            val expiry = PushJmap.expiryMillis(expires)
            if (active.optString("stage") == "active" && expiry != null &&
                System.currentTimeMillis() >= expiry - java.util.concurrent.TimeUnit.DAYS.toMillis(1)) {
                val id = active.getString("subscription_id")
                if (!PushEnrollmentStore.update(applicationContext, instance) {
                        if (it.optString("stage") != "active") return@update false
                        it.put("stage", "renew_unknown"); true
                    }) return@withContext Result.success()
                val renewed = PushJmap.call(applicationContext, account, "renew", org.json.JSONObject()
                    .put("id", id).put("expires", PushJmap.requestedExpiry()))
                check(renewed.optString("status") == "applied")
                val currentSubscription = requireNotNull(PushJmap.find(applicationContext, account, instance, endpoint,
                    active.getString("public_key"), active.getString("auth")))
                check(currentSubscription.optString("id") == id)
                val actualExpiry = currentSubscription.getString("expires")
                requireNotNull(PushJmap.expiryMillis(actualExpiry))
                PushEnrollmentStore.update(applicationContext, instance) {
                    if (it.optString("stage") != "renew_unknown") return@update false
                    it.put("expires", actualExpiry).put("stage", "active"); true
                }
                PushJmap.scheduleRenewal(applicationContext, instance, actualExpiry)
            }
            val before = PushEnrollmentStore.get(applicationContext, instance)?.optLong("hint_generation", 0) ?: 0
            if (NativeEngine.nativeWaitProviderRefresh() != 1) return@withContext Result.retry()
            PushEnrollmentStore.update(applicationContext, instance) {
                if (it.optLong("hint_generation", 0) == before) it.put("hint_pending", false)
                true
            }
            if (PushEnrollmentStore.get(applicationContext, instance)?.optBoolean("hint_pending", false) == true) {
                PushScheduler.enqueue(applicationContext, instance)
            }
            Result.success()
        } catch (_: Exception) {
            // State retains the unknown outcome for reconciliation, not an automatic replay.
            Result.failure()
        }
    }
}
