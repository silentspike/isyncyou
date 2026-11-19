package com.silentspike.isyncyou

import androidx.work.WorkInfo
import org.junit.Assert.*
import org.junit.Test
import org.json.JSONObject

class PushHintSchedulingTest {
    @Test fun hintAfterFinalPendingReadQueuesSuccessorAndBurstCoalesces() {
        // The first worker has already observed no pending hint, but WorkManager
        // still reports it RUNNING when the receiver persists the next generation.
        val states = mutableListOf(WorkInfo.State.RUNNING)
        assertTrue(PushHintScheduling.needsSuccessor(states))
        states += WorkInfo.State.BLOCKED // appended behind the running worker
        repeat(100) { assertFalse(PushHintScheduling.needsSuccessor(states)) }
        states[0] = WorkInfo.State.SUCCEEDED
        states[1] = WorkInfo.State.RUNNING
        // A new hint at the successor's own final read must also be covered.
        assertTrue(PushHintScheduling.needsSuccessor(states))
        states += WorkInfo.State.ENQUEUED
        assertFalse(PushHintScheduling.needsSuccessor(states))
    }

    @Test fun finishedOrFailedWorkDoesNotSuppressNewHint() {
        assertTrue(PushHintScheduling.needsSuccessor(listOf(WorkInfo.State.SUCCEEDED, WorkInfo.State.FAILED, WorkInfo.State.CANCELLED)))
    }

    @Test fun failedEnrollmentHasSeparateWorkScopeAndDoesNotSuppressHint() {
        val instance = "0123456789abcdef"
        assertNotEquals("provider-push:$instance", PushHintScheduling.workName(instance))
        // The hint query sees only its own tag/states, not the FAILED enrollment.
        assertTrue(PushHintScheduling.needsSuccessor(emptyList()))
        assertFalse(PushHintScheduling.needsSuccessor(listOf(WorkInfo.State.BLOCKED)))
    }

    @Test fun onlyMatchingGenerationAndCustodyCanClearPendingHint() {
        val state = JSONObject().put("account", "test").put("origin", "https://test.invalid")
            .put("stage", "active").put("hint_generation", 7).put("hint_pending", true)
        assertTrue(PushHintPolicy.mayClear(state, "test", "https://test.invalid", 7))
        state.put("hint_generation", 8)
        assertFalse(PushHintPolicy.mayClear(state, "test", "https://test.invalid", 7))
        assertFalse(PushHintPolicy.mayClear(state, "other", "https://test.invalid", 8))
        state.put("stage", "broker_revoked_logout")
        assertFalse(PushHintPolicy.mayClear(state, "test", "https://test.invalid", 8))
    }
}
