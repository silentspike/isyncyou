package com.silentspike.isyncyou

import android.content.Context
import android.content.Intent
import androidx.work.ExistingWorkPolicy
import androidx.work.Constraints
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkInfo
import androidx.work.WorkQuery
import androidx.work.workDataOf
import org.json.JSONObject
import org.unifiedpush.android.connector.FailedReason
import org.unifiedpush.android.connector.MessagingReceiver
import org.unifiedpush.android.connector.data.PushEndpoint
import org.unifiedpush.android.connector.data.PushMessage
import org.unifiedpush.android.connector.keys.KeyManager
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.ThreadPoolExecutor
import java.util.concurrent.TimeUnit

/** Only decrypted server hints and the broker's pending proof are accepted. */
@Suppress("DEPRECATION")
class UnifiedPushReceiver : MessagingReceiver() {
    private class Delivery(val pending: android.content.BroadcastReceiver.PendingResult) { var deferred = false }
    private val delivery = ThreadLocal<Delivery>()
    override fun getKeyManager(context: Context): KeyManager = WebPushKeyManager(context)

    override fun onReceive(context: Context, intent: Intent) {
        // Invalid external broadcasts or inaccessible Keystore records fail closed.
        val current = Delivery(goAsync())
        delivery.set(current)
        try { runCatching { super.onReceive(context, intent) } }
        finally {
            delivery.remove()
            if (!current.deferred) current.pending.finish()
        }
    }

    override fun onNewEndpoint(context: Context, endpoint: PushEndpoint, instance: String) {
        val keys = endpoint.pubKeySet ?: return
        if (endpoint.temporary || endpoint.url.length > 2048) return
        val changed = PushEnrollmentStore.update(context, instance) { state ->
            PushEndpointTransition.receive(state, endpoint.url, keys.pubKey, keys.auth)
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
            if (changed) {
                if (payload.optString("@type") == "StateChange") {
                    val current = delivery.get()
                    if (current != null) {
                        current.deferred = PushScheduler.enqueueHintAsync(context.applicationContext, instance) { current.pending.finish() }
                    } else {
                        PushScheduler.enqueueHintAsync(context.applicationContext, instance) {}
                    }
                }
                else PushScheduler.enqueue(context, instance)
            }
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
    // One waiting delivery bounds both memory and broadcast completion time.
    private val hintExecutor = ThreadPoolExecutor(1, 1, 0, TimeUnit.SECONDS, ArrayBlockingQueue(1))
    private var outstandingHintEnqueue: androidx.work.Operation? = null

    fun enqueueHintAsync(context: Context, instance: String, done: () -> Unit): Boolean = try {
        hintExecutor.execute { try { runCatching { enqueueHint(context, instance) } } finally { done() } }
        true
    } catch (_: java.util.concurrent.RejectedExecutionException) {
        // The encrypted pending hint remains for the periodic catch-up.
        false
    }
    // Serialize the query and completed enqueue among receivers in this process.
    // A running hint worker does not cover a hint arriving at its final read.
    @Synchronized
    fun enqueueHint(context: Context, instance: String) {
        // If a previous DB operation timed out, do not enqueue another until its
        // outcome is known: it may still commit after the timeout.
        outstandingHintEnqueue?.let {
            if (!it.result.isDone) return
            outstandingHintEnqueue = null
        }
        val manager = WorkManager.getInstance(context)
        val tag = "provider-push-hint:$instance"
        val query = WorkQuery.Builder.fromTags(listOf(tag))
            .addStates(listOf(WorkInfo.State.ENQUEUED, WorkInfo.State.BLOCKED)).build()
        val states = manager.getWorkInfos(query).get(2, TimeUnit.SECONDS).map { it.state }
        if (!PushHintScheduling.needsSuccessor(states)) return
        val request = OneTimeWorkRequestBuilder<PushEnrollmentWorker>()
            .setInputData(workDataOf("instance" to instance))
            .setConstraints(Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build())
            .addTag(tag)
            .build()
        val operation = manager.enqueueUniqueWork("provider-push:$instance", ExistingWorkPolicy.APPEND_OR_REPLACE, request)
        outstandingHintEnqueue = operation
        operation.result.get(2, TimeUnit.SECONDS)
        outstandingHintEnqueue = null
    }
    fun scheduleCatchUp(context: Context, instance: String) {
        // Migrate previously scheduled per-installation polling to one account pass.
        WorkManager.getInstance(context).cancelUniqueWork("provider-push-catch-up:$instance")
        ProviderCatchUpWorker.schedule(context)
    }

    fun cancel(context: Context, instance: String) {
        WorkManager.getInstance(context).cancelUniqueWork("provider-push-catch-up:$instance")
        WorkManager.getInstance(context).cancelUniqueWork("provider-push:$instance")
        WorkManager.getInstance(context).cancelAllWorkByTag(PushJmap.renewalTag(instance))
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

internal object PushHintScheduling {
    fun needsSuccessor(states: List<WorkInfo.State>): Boolean =
        states.none { it == WorkInfo.State.ENQUEUED || it == WorkInfo.State.BLOCKED }
}
