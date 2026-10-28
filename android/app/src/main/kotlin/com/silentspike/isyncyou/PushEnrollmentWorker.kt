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
                "destroy_unknown", "replacement_destroy_unknown" -> {
                    val previousStage = state.getString("stage")
                    val id = state.getString("subscription_id")
                    if (PushJmap.lookup(applicationContext, account, instance, id) == PushJmap.Lookup.Absent) {
                        PushEnrollmentStore.update(applicationContext, instance) {
                            if (it.optString("stage") != previousStage || it.optString("subscription_id") != id) return@update false
                            it.remove("subscription_id")
                            if (previousStage == "destroy_unknown") it.put("stage", "broker_bound_logout")
                            else it.put("replacement_from", "broker_bound").put("stage", "endpoint_changed")
                            true
                        }
                        if (previousStage == "replacement_destroy_unknown") PushScheduler.enqueue(applicationContext, instance)
                    }
                }
                "endpoint_changed" -> {
                    // Preserve both endpoints until old subscription and broker binding
                    // are confirmed removed. Unknown outcomes remain explicit.
                    check(state.optString("replacement_from") in setOf("active", "verification_pending", "broker_bound"))
                    val oldSubscription = state.optString("subscription_id", "")
                    if (oldSubscription.isNotEmpty()) {
                        if (!PushEnrollmentStore.update(applicationContext, instance) {
                                if (it.optString("stage") != "endpoint_changed") return@update false
                                it.put("stage", "replacement_destroy_unknown"); true
                            }) return@withContext Result.success()
                        val destroyed = PushJmap.call(applicationContext, account, "destroy", org.json.JSONObject().put("id", oldSubscription))
                        check(destroyed.optString("status") == "applied")
                    }
                    val expected = if (oldSubscription.isEmpty()) "endpoint_changed" else "replacement_destroy_unknown"
                    if (!PushEnrollmentStore.update(applicationContext, instance) {
                            if (it.optString("stage") != expected) return@update false
                            it.put("stage", "replacement_revoke_unknown"); true
                        }) return@withContext Result.success()
                    val revoked = PushBroker.operation(applicationContext, account, endpoint, "revoke")
                    check(revoked.optString("status") == "revoked")
                    PushEnrollmentStore.update(applicationContext, instance) {
                        if (it.optString("stage") != "replacement_revoke_unknown") return@update false
                        val replacement = it.getJSONObject("replacement")
                        it.put("endpoint", replacement.getString("endpoint"))
                            .put("public_key", replacement.getString("public_key"))
                            .put("auth", replacement.getString("auth"))
                            .put("stage", "endpoint_received")
                        listOf("replacement", "replacement_from", "subscription_id", "expires", "verify_id", "verify_code").forEach { key -> it.remove(key) }
                        true
                    }
                    PushScheduler.enqueue(applicationContext, instance)
                }
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
                    ?: PushJmap.findKnown(applicationContext, account, instance, id)?.optString("expires", "")
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
                val verifiedId = afterCreate.optString("verify_id", "")
                val existing = if (verifiedId.isNotEmpty() && afterCreate.optString("verify_code", "").isNotEmpty()) {
                    PushJmap.findKnown(applicationContext, account, instance, verifiedId)
                } else null
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
            if (active.optString("stage") == "renew_unknown" && expiry != null) {
                val id = active.getString("subscription_id")
                val observed = PushJmap.findKnown(applicationContext, account, instance, id)
                val observedExpiry = observed?.optString("expires", "") ?: ""
                val observedMillis = PushJmap.expiryMillis(observedExpiry)
                if (observedMillis != null && observedMillis > expiry) {
                    PushEnrollmentStore.update(applicationContext, instance) {
                        if (it.optString("stage") != "renew_unknown") return@update false
                        it.put("expires", observedExpiry).put("stage", "active"); true
                    }
                    PushJmap.scheduleRenewal(applicationContext, instance, observedExpiry)
                }
            }
            if (active.optString("stage") == "active" && expiry != null &&
                System.currentTimeMillis() < expiry - java.util.concurrent.TimeUnit.DAYS.toMillis(1)) {
                PushJmap.scheduleRenewal(applicationContext, instance, expires)
            }
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
                val currentSubscription = requireNotNull(PushJmap.findKnown(applicationContext, account, instance, id))
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
            if (NativeEngine.nativeWaitProviderRefresh(account) != 1) return@withContext Result.retry()
            PushEnrollmentStore.update(applicationContext, instance) {
                if (it.optLong("hint_generation", 0) == before) it.put("hint_pending", false)
                it.put("last_sync_at", System.currentTimeMillis() / 1000)
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
