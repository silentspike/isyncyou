package com.silentspike.isyncyou

import android.content.Context
import android.content.Intent
import androidx.work.ExistingWorkPolicy
import androidx.work.Constraints
import androidx.work.NetworkType
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.PeriodicWorkRequestBuilder
import java.util.concurrent.TimeUnit
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.workDataOf
import org.json.JSONObject
import org.unifiedpush.android.connector.FailedReason
import org.unifiedpush.android.connector.MessagingReceiver
import org.unifiedpush.android.connector.data.PushEndpoint
import org.unifiedpush.android.connector.data.PushMessage
import org.unifiedpush.android.connector.keys.KeyManager

/** Only decrypted server hints and the broker's pending proof are accepted. */
@Suppress("DEPRECATION")
class UnifiedPushReceiver : MessagingReceiver() {
    override fun getKeyManager(context: Context): KeyManager = WebPushKeyManager(context)

    override fun onReceive(context: Context, intent: Intent) {
        // Invalid external broadcasts or inaccessible Keystore records fail closed.
        runCatching { super.onReceive(context, intent) }
    }

    override fun onNewEndpoint(context: Context, endpoint: PushEndpoint, instance: String) {
        val keys = endpoint.pubKeySet ?: return
        if (endpoint.temporary || endpoint.url.length > 2048) return
        val changed = PushEnrollmentStore.update(context, instance) { state ->
            val old = state.optString("endpoint", "")
            if (old.isNotEmpty() && old != endpoint.url) {
                if (state.optString("stage") == "endpoint_changed") return@update false
                state.put("replacement_from", state.optString("stage"))
                    .put("replacement", JSONObject().put("endpoint", endpoint.url)
                        .put("public_key", keys.pubKey).put("auth", keys.auth))
                    .put("stage", "endpoint_changed")
            } else if (old.isEmpty()) {
                state.put("endpoint", endpoint.url).put("public_key", keys.pubKey).put("auth", keys.auth)
                    .put("stage", "endpoint_received")
            } else return@update false
            true
        }
        if (changed) PushScheduler.enqueue(context, instance)
    }

    override fun onMessage(context: Context, message: PushMessage, instance: String) {
        if (message.content.size !in 1..8192) return
        if (message.decrypted) {
            val payload = runCatching { JSONObject(String(message.content, Charsets.UTF_8)) }.getOrNull() ?: return
            val changed = PushEnrollmentStore.update(context, instance) { state ->
                when (payload.optString("@type")) {
                    "StateChange" -> {
                        // A genuine encrypted state change on this subscription proves
                        // that the server accepted an otherwise unknown verification.
                        if (state.optString("stage") == "verify_unknown") {
                            state.put("stage", "active"); state.remove("verify_id"); state.remove("verify_code")
                        }
                        state.put("degraded", false).put("hint_pending", true)
                            .put("hint_generation", state.optLong("hint_generation", 0) + 1)
                            .put("last_hint_at", System.currentTimeMillis() / 1000)
                        true
                    }
                    "PushVerification" -> {
                        val subscription = payload.optString("pushSubscriptionId", "")
                        val code = payload.optString("verificationCode", "")
                        if (subscription.length !in 1..512 || code.length !in 1..512) return@update false
                        if (state.optString("stage") == "active" ||
                            (state.optString("verify_id") == subscription && state.optString("verify_code") == code)) return@update false
                        // This may arrive before PushSubscription/set returns; retain it securely.
                        state.put("verify_id", subscription).put("verify_code", code)
                        true
                    }
                    else -> false
                }
            }
            if (changed) PushScheduler.enqueue(context, instance, queueAfter = payload.optString("@type") != "StateChange")
        } else {
            val proof = runCatching { JSONObject(String(message.content, Charsets.UTF_8)) }.getOrNull() ?: return
            val nonce = proof.optString("nonce", "")
            if (proof.length() != 2 || proof.optString("type") != "push-binding-proof" ||
                !nonce.matches(Regex("[A-Za-z0-9_-]{43}"))) return
            val changed = PushEnrollmentStore.update(context, instance) { state ->
                if (state.optString("stage") !in setOf("awaiting_proof", "delivery_unknown")) return@update false
                state.put("proof_nonce", nonce).put("stage", "proof_received")
                true
            }
            if (changed) PushScheduler.enqueue(context, instance)
        }
    }

    override fun onRegistrationFailed(context: Context, reason: FailedReason, instance: String) = degrade(context, instance)
    override fun onUnregistered(context: Context, instance: String) = degrade(context, instance)
    override fun onTempUnavailable(context: Context, instance: String) = degrade(context, instance)

    private fun degrade(context: Context, instance: String) {
        if (PushEnrollmentStore.update(context, instance) { state -> state.put("degraded", true); true }) PushScheduler.enqueue(context, instance)
    }
}

internal object PushScheduler {
    fun scheduleCatchUp(context: Context, instance: String) {
        val request = PeriodicWorkRequestBuilder<PushEnrollmentWorker>(15, TimeUnit.MINUTES)
            .setInputData(workDataOf("instance" to instance))
            .setConstraints(Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build())
            .build()
        WorkManager.getInstance(context).enqueueUniquePeriodicWork("provider-push-catch-up:$instance", ExistingPeriodicWorkPolicy.KEEP, request)
    }

    fun cancel(context: Context, instance: String) {
        WorkManager.getInstance(context).cancelUniqueWork("provider-push-catch-up:$instance")
        WorkManager.getInstance(context).cancelUniqueWork("provider-push:$instance")
    }
    fun enqueue(context: Context, instance: String, queueAfter: Boolean = true) {
        val request = OneTimeWorkRequestBuilder<PushEnrollmentWorker>()
            .setInputData(workDataOf("instance" to instance))
            .setConstraints(Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build())
            .build()
        WorkManager.getInstance(context).enqueueUniqueWork("provider-push:$instance",
            if (queueAfter) ExistingWorkPolicy.APPEND_OR_REPLACE else ExistingWorkPolicy.KEEP, request)
    }
}
