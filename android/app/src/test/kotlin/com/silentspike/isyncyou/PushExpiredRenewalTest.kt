package com.silentspike.isyncyou

import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test

class PushExpiredRenewalTest {
    private fun state() = JSONObject().put("stage", "renew_unknown").put("subscription_id", "old")
        .put("expires", "2020-01-01T00:00:00Z").put("account", "account")
        .put("origin", "https://provider.example").put("endpoint", "https://push.example/topic")
        .put("auth", "preserved")

    private fun recover(s: JSONObject, snapshot: JSONObject = JSONObject(s.toString()), absent: Boolean = true, empty: Boolean = true) =
        PushExpiredRenewal.recover(s, snapshot, "account", "https://provider.example", "https://push.example/topic", 1700000000000L, absent, empty)

    private fun response(list: JSONArray) = JSONObject().put("methodResponses", JSONArray().put(
        JSONArray().put("PushSubscription/get").put(JSONObject().put("state", "v1")
            .put("list", list).put("notFound", JSONArray())).put("c1")))

    @Test fun expiredExactAbsencePreservesOriginalAndCredentials() {
        val s = state()
        assertTrue(recover(s))
        assertEquals("broker_bound", s.getString("stage"))
        assertEquals("old", s.getString("expired_subscription_id"))
        assertEquals("2020-01-01T00:00:00Z", s.getString("expired_subscription_expires"))
        assertEquals("preserved", s.getString("auth"))
        assertFalse(s.has("subscription_id"))
    }

    @Test fun refusesPresentConflictExpiryReplacementAndChangedState() {
        assertFalse(recover(state(), absent = false))
        assertFalse(recover(state(), empty = false))
        assertFalse(recover(state().put("expires", "2099-01-01T00:00:00Z")))
        assertFalse(recover(state().put("replacement", JSONObject())))
        assertFalse(recover(state().put("stage", "verify_unknown")))
        for (key in listOf("stage", "subscription_id", "expires", "account", "origin", "endpoint")) {
            val before = state()
            val after = JSONObject(before.toString()).put(key, "changed")
            assertFalse(recover(after, before))
        }
    }

    @Test fun fullGetRequiresValidCompleteResponseAndNoSameInstallation() {
        assertTrue(PushJmap.noInstallation(response(JSONArray()), "installation"))
        val conflict = JSONArray().put(JSONObject().put("id", "x").put("deviceClientId", "installation"))
        assertFalse(PushJmap.noInstallation(response(conflict), "installation"))
        assertThrows(Exception::class.java) { PushJmap.noInstallation(JSONObject(), "installation") }
        assertThrows(Exception::class.java) {
            PushJmap.noInstallation(response(JSONArray().put(JSONObject().put("id", "x"))), "installation")
        }
    }
}
