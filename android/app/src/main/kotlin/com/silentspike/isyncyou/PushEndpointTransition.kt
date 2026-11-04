package com.silentspike.isyncyou

import org.json.JSONObject

/** Keeps unknown remote outcomes intact while retaining the distributor's latest endpoint. */
internal object PushEndpointTransition {
    private val replaceable = setOf("active", "verification_pending", "broker_bound")

    fun receive(state: JSONObject, url: String, publicKey: String, auth: String): Boolean {
        val old = state.optString("endpoint", "")
        val stage = state.optString("stage")
        val keyChanged = old == url && (state.optString("public_key") != publicKey || state.optString("auth") != auth)
        if (old.isEmpty() || (stage == "endpoint_received" && (old != url || keyChanged))) {
            state.put("endpoint", url).put("public_key", publicKey).put("auth", auth)
                .put("stage", "endpoint_received")
            state.remove("replacement"); state.remove("replacement_from"); state.remove("degraded")
            return true
        }
        if (old == url && !keyChanged) {
            if (!state.optBoolean("degraded", false)) return false
            state.put("degraded", false)
            return true
        }
        state.put("replacement", JSONObject().put("endpoint", url).put("public_key", publicKey).put("auth", auth))
        if (stage in replaceable) state.put("replacement_from", stage).put("stage", "endpoint_changed")
        return true
    }

    fun promote(state: JSONObject): Boolean {
        val stage = state.optString("stage")
        if (!state.has("replacement") || stage !in replaceable) return false
        state.put("replacement_from", stage).put("stage", "endpoint_changed")
        return true
    }

    fun claimRenewReplacement(state: JSONObject, subscriptionId: String): Boolean {
        if (state.optString("stage") !in setOf("renew_unknown", "verify_unknown") || !state.has("replacement") ||
            subscriptionId.isEmpty() || state.optString("subscription_id") != subscriptionId) return false
        state.put("stage", "replacement_destroy_unknown")
        return true
    }

    fun reconcileDestroyed(state: JSONObject, previous: String, subscriptionId: String, absent: Boolean): Boolean {
        if (!absent || state.optString("stage") != previous || state.optString("subscription_id") != subscriptionId ||
            previous !in setOf("destroy_unknown", "replacement_destroy_unknown")) return false
        state.remove("subscription_id")
        if (previous == "destroy_unknown") state.put("stage", "broker_bound_logout")
        else state.put("replacement_from", "broker_bound").put("stage", "endpoint_changed")
        return true
    }

    fun beginDelivery(current: JSONObject, shown: JSONObject, account: String, origin: String): Boolean {
        if (current.optString("stage") != "credentials_ready" || current.has("replacement") ||
            current.optString("account") != account || current.optString("origin") != origin ||
            listOf("endpoint", "username", "password").any { current.optString(it) != shown.optString(it) } ||
            current.optLong("expires", 0) != shown.optLong("expires", 0) ||
            current.optLong("expires", 0) <= System.currentTimeMillis() / 1000) return false
        current.put("stage", "awaiting_proof")
        return true
    }

    fun reconcileStatus(state: JSONObject, previous: String, account: String, origin: String, endpoint: String, observed: String,
                        fencedOperation: Boolean = false, expectedOperation: String? = null): Boolean {
        if (state.optString("stage") != previous || state.optString("account") != account ||
            state.optString("origin") != origin || state.optString("endpoint") != endpoint) return false
        when {
            previous == "confirm_unknown" && observed == "active" -> {
                state.remove("password"); state.remove("proof_nonce"); state.put("stage", "broker_bound")
            }
            previous in setOf("revoke_unknown", "replacement_revoke_unknown") && observed == "active" &&
                fencedOperation && expectedOperation != null && state.optString("revoke_operation_id") == expectedOperation -> {
                state.remove("revoke_operation_id")
                state.put("stage", if (previous == "revoke_unknown") "broker_bound_logout" else "replacement_revoke_retry_ready")
            }
            previous == "revoke_unknown" && observed == "retired" -> state.put("stage", "broker_revoked_logout")
            (previous == "replacement_revoke_unknown" || previous == "prepare_unknown" ||
                (previous in setOf("credentials_ready", "awaiting_proof", "delivery_unknown", "proof_received", "confirm_unknown") &&
                    state.optLong("expires", 0) > 0 && System.currentTimeMillis() / 1000 >= state.optLong("expires", 0) + 180)) && observed == "retired" -> {
                val replacement = state.optJSONObject("replacement")
                if (replacement == null || replacement.optString("endpoint") == endpoint) {
                    // A retired topic cannot be prepared again. Require a fresh
                    // distributor registration rather than replaying this URL.
                    listOf("replacement", "replacement_from", "subscription_id", "expires", "verify_id", "verify_code", "username", "password", "proof_nonce", "revoke_operation_id").forEach { state.remove(it) }
                    state.put("stage", "prepare_cancelled")
                    return true
                }
                state.put("endpoint", replacement.getString("endpoint"))
                    .put("public_key", replacement.getString("public_key"))
                    .put("auth", replacement.getString("auth")).put("stage", "endpoint_received")
                listOf("replacement", "replacement_from", "subscription_id", "expires", "verify_id", "verify_code", "username", "password", "proof_nonce", "revoke_operation_id").forEach { state.remove(it) }
            }
            else -> return false
        }
        return true
    }
}

/** A failed or lost compensation response remains claimed for read-only reconciliation. */
internal object PushRenewReplacement {
    fun dispatch(claim: () -> Boolean, destroy: () -> Unit): Boolean {
        if (!claim()) return false
        destroy()
        return true
    }
}

internal object PushStatusRead {
    fun reconcile(read: () -> JSONObject, apply: (JSONObject) -> Boolean): Boolean = apply(read())
}
