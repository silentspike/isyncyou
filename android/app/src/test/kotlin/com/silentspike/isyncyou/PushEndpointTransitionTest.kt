package com.silentspike.isyncyou

import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test

class PushEndpointTransitionTest {
    private fun state(stage: String) = JSONObject().put("stage", stage).put("endpoint", "https://old.example/topic")
        .put("account", "fixture").put("origin", "https://provider.example")
        .put("username", "user").put("password", "secret").put("public_key", "old-key").put("auth", "old-auth")
        .put("expires", System.currentTimeMillis() / 1000 + 300)

    @Test fun endpointBeforePrepareIsReplacedWithoutBrokerEffects() {
        val s = state("endpoint_received")
        assertTrue(PushEndpointTransition.receive(s, "https://new.example/topic", "key", "auth"))
        assertEquals("endpoint_received", s.getString("stage"))
        assertEquals("https://new.example/topic", s.getString("endpoint"))
        assertFalse(s.has("replacement"))
    }

    @Test fun unknownOrProvisionalEffectsStayBoundToOriginalEndpoint() {
        for (stage in listOf("prepare_unknown", "credentials_ready", "awaiting_proof", "delivery_unknown", "proof_received", "confirm_unknown", "create_unknown", "verify_unknown", "renew_unknown", "replacement_revoke_unknown")) {
            val s = state(stage)
            assertTrue(PushEndpointTransition.receive(s, "https://new.example/topic", "key", "auth"))
            assertEquals(stage, s.getString("stage"))
            assertEquals("https://old.example/topic", s.getString("endpoint"))
            assertEquals("https://new.example/topic", s.getJSONObject("replacement").getString("endpoint"))
            assertFalse(PushEndpointTransition.promote(s))
        }
    }

    @Test fun reconciledBindingPromotesPendingReplacement() {
        for (stage in listOf("active", "verification_pending", "broker_bound")) {
            val s = state("confirm_unknown")
            PushEndpointTransition.receive(s, "https://new.example/topic", "key", "auth")
            s.put("stage", stage)
            assertTrue(PushEndpointTransition.promote(s))
            assertEquals("endpoint_changed", s.getString("stage"))
            assertEquals(stage, s.getString("replacement_from"))
        }
    }

    @Test fun endpointChangeWhileCredentialsDialogOpenCannotDispatchOldBinding() {
        val shown = state("credentials_ready")
        val current = JSONObject(shown.toString())
        PushEndpointTransition.receive(current, "https://new.example/topic", "new-key", "new-auth")
        assertFalse(PushEndpointTransition.beginDelivery(current, shown, "fixture", "https://provider.example"))
        assertEquals("credentials_ready", current.getString("stage"))
        assertEquals("https://new.example/topic", current.getJSONObject("replacement").getString("endpoint"))
        assertTrue(PushEndpointTransition.beginDelivery(shown, JSONObject(shown.toString()), "fixture", "https://provider.example"))
    }

    @Test fun statusReadbackOnlyAdvancesExactConfirmedOutcomes() {
        val account = "fixture"; val origin = "https://provider.example"; val endpoint = "https://old.example/topic"
        for (stage in listOf("confirm_unknown", "revoke_unknown", "replacement_revoke_unknown")) {
            val s = state(stage)
            s.put("replacement", JSONObject().put("endpoint", "https://new.example/topic").put("public_key", "new").put("auth", "auth"))
            val before = s.toString()
            for (observation in listOf("unknown", "conflict", "provisional", "delivered")) {
                assertFalse(PushEndpointTransition.reconcileStatus(s, stage, account, origin, endpoint, observation))
                assertEquals(before, s.toString())
            }
            assertFalse(PushEndpointTransition.reconcileStatus(s, stage, "other", origin, endpoint, "absent"))
            assertEquals(before, s.toString())
        }
        val confirm = state("confirm_unknown")
        assertTrue(PushEndpointTransition.reconcileStatus(confirm, "confirm_unknown", account, origin, endpoint, "active"))
        assertEquals("broker_bound", confirm.getString("stage"))
        val logout = state("revoke_unknown")
        assertTrue(PushEndpointTransition.reconcileStatus(logout, "revoke_unknown", account, origin, endpoint, "retired"))
        assertEquals("broker_revoked_logout", logout.getString("stage"))
        val replacement = state("replacement_revoke_unknown")
        replacement.put("replacement", JSONObject().put("endpoint", "https://new.example/topic").put("public_key", "new").put("auth", "auth"))
        assertTrue(PushEndpointTransition.reconcileStatus(replacement, "replacement_revoke_unknown", account, origin, endpoint, "retired"))
        assertEquals("endpoint_received", replacement.getString("stage"))
        assertEquals("https://new.example/topic", replacement.getString("endpoint"))
    }

    @Test fun sameUrlWithNewKeysRequiresReplacementNotSilentAcceptance() {
        val s = state("active")
        assertTrue(PushEndpointTransition.receive(s, "https://old.example/topic", "new-key", "new-auth"))
        assertEquals("endpoint_changed", s.getString("stage"))
        assertEquals("old-key", s.getString("public_key"))
        assertEquals("new-key", s.getJSONObject("replacement").getString("public_key"))
    }

    @Test fun expiredDeliveryReplacementRequiresRetiredFence() {
        for (stage in listOf("credentials_ready", "awaiting_proof", "delivery_unknown", "proof_received", "confirm_unknown")) {
            val s = state(stage).put("expires", System.currentTimeMillis() / 1000 - 181)
            s.put("replacement", JSONObject().put("endpoint", "https://new.example/topic").put("public_key", "new").put("auth", "auth"))
            assertFalse(PushEndpointTransition.reconcileStatus(s, stage, "fixture", "https://provider.example", "https://old.example/topic", "absent"))
            assertTrue(PushEndpointTransition.reconcileStatus(s, stage, "fixture", "https://provider.example", "https://old.example/topic", "retired"))
            assertEquals("endpoint_received", s.getString("stage"))
        }
    }

    @Test fun retiredSameUrlRequiresFreshDistributorRegistration() {
        val s = state("delivery_unknown").put("expires", System.currentTimeMillis() / 1000 - 181)
        s.put("replacement", JSONObject().put("endpoint", "https://old.example/topic").put("public_key", "new").put("auth", "auth"))
        assertTrue(PushEndpointTransition.reconcileStatus(s, "delivery_unknown", "fixture", "https://provider.example", "https://old.example/topic", "retired"))
        assertEquals("prepare_cancelled", s.getString("stage"))
        assertFalse(s.has("password"))
        assertFalse(s.has("replacement"))
    }

    @Test fun uncertainPrepareCanAdvanceOnlyAfterExactRetirement() {
        val s = state("prepare_unknown")
        s.put("replacement", JSONObject().put("endpoint", "https://new.example/topic").put("public_key", "new").put("auth", "auth"))
        assertFalse(PushEndpointTransition.reconcileStatus(s, "prepare_unknown", "fixture", "https://provider.example", "https://old.example/topic", "absent"))
        assertTrue(PushEndpointTransition.reconcileStatus(s, "prepare_unknown", "fixture", "https://provider.example", "https://old.example/topic", "retired"))
        assertEquals("endpoint_received", s.getString("stage"))
    }

    @Test fun revokeActiveNeedsExactSignedOperationFenceAndExplicitRetry() {
        val endpoint = "https://old.example/topic"
        val legacy = state("revoke_unknown")
        assertFalse(PushEndpointTransition.reconcileStatus(legacy, "revoke_unknown", "fixture", "https://provider.example", endpoint, "active"))
        val id = "a".repeat(32)
        val s = state("revoke_unknown").put("revoke_operation_id", id)
        assertFalse(PushEndpointTransition.reconcileStatus(s, "revoke_unknown", "fixture", "https://provider.example", endpoint, "active", true, "b".repeat(32)))
        assertTrue(PushEndpointTransition.reconcileStatus(s, "revoke_unknown", "fixture", "https://provider.example", endpoint, "active", true, id))
        assertEquals("broker_bound_logout", s.getString("stage"))
        assertFalse(s.has("revoke_operation_id"))
        val replacement = state("replacement_revoke_unknown").put("revoke_operation_id", id)
        replacement.put("replacement", JSONObject().put("endpoint", "https://new.example/topic").put("public_key", "new").put("auth", "auth"))
        assertTrue(PushEndpointTransition.reconcileStatus(replacement, "replacement_revoke_unknown", "fixture", "https://provider.example", endpoint, "active", true, id))
        assertEquals("replacement_revoke_retry_ready", replacement.getString("stage"))
    }

    @Test fun lostStatusResponseDoesNotChangeStateOrDispatchRevoke() {
        val id = "c".repeat(32)
        val s = state("revoke_unknown").put("revoke_operation_id", id)
        val before = s.toString()
        var reads = 0
        var applies = 0
        try {
            PushStatusRead.reconcile(read = { reads++; throw IllegalStateException("response lost") }, apply = { applies++; true })
            fail("expected lost response")
        } catch (_: IllegalStateException) { }
        assertEquals(before, s.toString())
        assertEquals(1, reads)
        assertEquals(0, applies)
        assertTrue(PushStatusRead.reconcile(read = { reads++; JSONObject().put("binding", "active").put("operation_fenced", true) }, apply = {
            applies++
            PushEndpointTransition.reconcileStatus(s, "revoke_unknown", "fixture", "https://provider.example", "https://old.example/topic", it.getString("binding"), it.getBoolean("operation_fenced"), id)
        }))
        assertEquals("broker_bound_logout", s.getString("stage"))
        assertEquals(2, reads)
        assertEquals(1, applies)
    }

    @Test fun uncertainRenewClaimsOneDistinctReplacementDestroy() {
        for (stage in listOf("renew_unknown", "verify_unknown")) {
            val s = state(stage).put("subscription_id", "old-subscription")
            s.put("replacement", JSONObject().put("endpoint", "https://new.example/topic").put("public_key", "new").put("auth", "auth"))
            assertFalse(PushEndpointTransition.claimRenewReplacement(s, "different"))
            assertEquals(stage, s.getString("stage"))
            assertTrue(PushEndpointTransition.claimRenewReplacement(s, "old-subscription"))
            assertEquals("replacement_destroy_unknown", s.getString("stage"))
            assertFalse(PushEndpointTransition.claimRenewReplacement(s, "old-subscription"))
        }
    }

    @Test fun lostDestroyResponseIsReconciledWithoutSecondDispatch() {
        for (stage in listOf("renew_unknown", "verify_unknown")) {
            val s = state(stage).put("subscription_id", "old-subscription")
            s.put("replacement", JSONObject().put("endpoint", "https://new.example/topic").put("public_key", "new").put("auth", "auth"))
            var calls = 0
            try {
                PushRenewReplacement.dispatch({ PushEndpointTransition.claimRenewReplacement(s, "old-subscription") }) {
                    calls++; throw IllegalStateException("response lost")
                }
                fail("expected lost response")
            } catch (_: IllegalStateException) { }
            assertEquals("replacement_destroy_unknown", s.getString("stage"))
            assertFalse(PushRenewReplacement.dispatch({ PushEndpointTransition.claimRenewReplacement(s, "old-subscription") }) { calls++ })
            assertEquals(1, calls)
            assertFalse(PushEndpointTransition.reconcileDestroyed(s, "replacement_destroy_unknown", "old-subscription", absent = false))
            assertTrue(PushEndpointTransition.reconcileDestroyed(s, "replacement_destroy_unknown", "old-subscription", absent = true))
            assertEquals("endpoint_changed", s.getString("stage"))
            assertFalse(PushEndpointTransition.reconcileDestroyed(s, "replacement_destroy_unknown", "old-subscription", absent = true))
        }
    }
}
