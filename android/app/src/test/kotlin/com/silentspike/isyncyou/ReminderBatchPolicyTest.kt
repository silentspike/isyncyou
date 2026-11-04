package com.silentspike.isyncyou

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.json.JSONObject
import org.junit.Test

class ReminderBatchPolicyTest {
    @Test fun unsupportedResourceDoesNotStarveSecondAccount() {
        val visited = mutableListOf<String>()
        val batch = ReminderBatchPolicy.run(listOf("first", "second")) { account ->
            visited += account
            ReminderAccountOutcome(successful = true, unsupported = account == "first")
        }
        assertEquals(listOf("first", "second"), visited)
        assertEquals(ReminderBatchOutcome("partial_coverage", false, ""), batch)
    }

    @Test fun failedAccountDoesNotPreventLaterAccountAndIsRetried() {
        val visited = mutableListOf<String>()
        val batch = ReminderBatchPolicy.run(listOf("first", "second")) { account ->
            visited += account
            if (account == "first") ReminderAccountOutcome(false, reason = "reminder_read")
            else ReminderAccountOutcome(true)
        }
        assertEquals(listOf("first", "second"), visited)
        assertEquals(ReminderBatchOutcome("partial_coverage", true, "reminder_read"), batch)
    }

    @Test fun thrownAccountFailureStillRunsRemainingAccount() {
        val visited = mutableListOf<String>()
        val batch = ReminderBatchPolicy.run(listOf("first", "second")) { account ->
            visited += account
            if (account == "first") error("transient")
            ReminderAccountOutcome(true)
        }
        assertEquals(listOf("first", "second"), visited)
        assertEquals(ReminderBatchOutcome("partial_coverage", true, ""), batch)
    }

    @Test fun partiallySuccessfulAccountRemainsPartialAndRetries() {
        val batch = ReminderBatchPolicy.run(listOf("one")) {
            ReminderAccountOutcome(true, reason = "reminder_sync", retry = true)
        }
        assertEquals(ReminderBatchOutcome("partial_coverage", true, "reminder_sync"), batch)
    }

    @Test fun noSuccessfulCollectionIsUnavailableAndRetried() {
        val batch = ReminderBatchPolicy.run(listOf("one")) {
            ReminderAccountOutcome(false, reason = "reminder_sync", retry = true)
        }
        assertEquals(ReminderBatchOutcome("unavailable", true, "reminder_sync"), batch)
    }

    @Test fun unreadableRecordDoesNotHideOtherRecords() {
        val (records, unreadable) = readReminderRecords(listOf("bad", "good")) {
            if (it == "bad") error("corrupt") else JSONObject().put("account", "good")
        }
        assertTrue(unreadable)
        assertEquals(listOf("good"), records.map { it.first })
    }

    @Test fun invalidateOnlyOwnedIncompleteCollections() {
        val tag = "a".repeat(64)
        val key = "r_$tag"
        fun record(account: String, collection: String) = JSONObject()
            .put("account", account).put("collection", collection).put("href", "x")
            .put("occurrence_start", "x").put("occurrence_end", "x")
            .put("window_start", 1).put("window_end", 2).put("floating_tzid", JSONObject.NULL)
            .put("uid", "u").put("recurrence_id", "r").put("alarm_id", "a")
            .put("trigger_at", 1).put("notification_tag", tag)
        assertTrue(invalidReminderScope(record("a", "revoked"), key, "a", setOf("good")))
        assertFalse(invalidReminderScope(record("a", "good"), key, "a", setOf("good")))
        assertFalse(invalidReminderScope(record("b", "revoked"), key, "a", setOf("good")))
        assertFalse(invalidReminderScope(record("a", ""), key, "a", setOf("good")))
        assertFalse(invalidReminderScope(record("a", "revoked").put("new_field", true), key, "a", setOf("good")))
        assertFalse(invalidReminderScope(record("a", "revoked"), "r_" + "b".repeat(64), "a", setOf("good")))
    }

    @Test(expected = kotlinx.coroutines.CancellationException::class)
    fun cancellationStopsRatherThanBecomingPartialFailure() {
        ReminderBatchPolicy.run(listOf("first", "second")) { throw kotlinx.coroutines.CancellationException("stopped") }
    }
}
