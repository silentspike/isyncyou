package com.silentspike.isyncyou

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class PushDispatchGateTest {
    @Test fun challengeFailureDoesNotClaimOrDispatch() {
        val events = mutableListOf<String>()
        val result = runCatching { PushDispatchGate.run<Int, Int>(
            { events.add("challenge"); error("unavailable") },
            { events.add("claim"); true },
            { events.add("dispatch"); it },
        ) }
        assertTrue(result.isFailure)
        assertEquals(listOf("challenge"), events)
    }

    @Test fun failedClaimNeverDispatchesAndSuccessIsOrdered() {
        val events = mutableListOf<String>()
        assertTrue(runCatching { PushDispatchGate.run(
            { events.add("challenge"); 7 }, { events.add("claim"); false }, { events.add("dispatch"); it },
        ) }.isFailure)
        assertEquals(listOf("challenge", "claim"), events)
        events.clear()
        val result = PushDispatchGate.run(
            { events.add("challenge"); 7 }, { events.add("claim"); true }, { events.add("dispatch"); it },
        )
        assertEquals(7, result)
        assertEquals(listOf("challenge", "claim", "dispatch"), events)
    }
}
