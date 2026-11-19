package com.silentspike.isyncyou

import androidx.work.WorkInfo
import org.junit.Assert.*
import org.junit.Test

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
}
