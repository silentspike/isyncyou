package com.silentspike.isyncyou

import org.junit.Assert.assertEquals
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

    @Test(expected = kotlinx.coroutines.CancellationException::class)
    fun cancellationStopsRatherThanBecomingPartialFailure() {
        ReminderBatchPolicy.run(listOf("first", "second")) { throw kotlinx.coroutines.CancellationException("stopped") }
    }
}
