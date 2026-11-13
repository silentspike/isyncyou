package com.silentspike.isyncyou

import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test

class PushLogoutCleanupTest {
    private class Fixture {
        var stored: JSONObject? = JSONObject().put("distributor", "ready")
        val work = mutableSetOf("old", "other")
        val enrollments = mutableSetOf("old", "other")
        val keys = mutableSetOf("old", "other")
        var calls = 0
        var failAt = ""
        fun run() {
            val snapshot = JSONObject(stored!!.toString())
            PushLogoutCleanup.resume(snapshot, { stored = JSONObject(it.toString()) }, {
                calls++
                if (failAt == "sdk") error("interrupted")
            }, {
                enrollments.remove("old")
                if (failAt == "enrollment") error("interrupted")
                work.remove("old")
                if (failAt == "work") error("interrupted")
                keys.remove("old")
                if (failAt == "key") error("interrupted")
            }, { stored = null })
        }
        fun assertScoped() {
            assertEquals(setOf("other"), enrollments)
            assertEquals(setOf("other"), work)
            assertEquals(setOf("other"), keys)
        }
    }

    @Test fun resumesAfterEveryLocalStepAndPreservesOtherInstallation() {
        for (point in listOf("enrollment", "work", "key")) {
            val f = Fixture()
            f.failAt = point
            assertTrue(runCatching { f.run() }.isFailure)
            f.failAt = ""
            f.run()
            assertEquals(1, f.calls)
            assertNull(f.stored)
            f.assertScoped()
        }
    }

    @Test fun interruptedSdkCallStaysUnknownAndIsNeverReplayed() {
        val f = Fixture()
        f.failAt = "sdk"
        f.run()
        assertEquals("unknown", f.stored!!.getString("distributor"))
        assertTrue(f.stored!!.getBoolean("local_done"))
        f.failAt = ""
        f.run()
        assertEquals(1, f.calls)
        assertEquals("unknown", f.stored!!.getString("distributor"))
        f.assertScoped()
    }

    @Test fun crashAfterUnknownWasPersistedDoesNotInvokeSdkOnRecovery() {
        val f = Fixture()
        f.stored!!.put("distributor", "unknown")
        f.run()
        assertEquals(0, f.calls)
        assertEquals("unknown", f.stored!!.getString("distributor"))
        f.assertScoped()
    }

    @Test fun invalidMarkerFailsBeforeAnySideEffect() {
        val f = Fixture()
        f.stored!!.put("distributor", "invalid")
        assertTrue(runCatching { f.run() }.isFailure)
        assertEquals(0, f.calls)
        assertEquals(setOf("old", "other"), f.work)
    }

    @Test fun providerLogoutRecoveryRetainsOriginalGenerationAfterCleanupAndCompletionCrash() {
        var stored: JSONObject? = JSONObject().put("generation", "original")
        var cleanupCalls = 0
        var finished = false
        var providerCompleted = false
        var failAfterProvider = true
        fun run() {
            val state = JSONObject(stored!!.toString())
            PushLogoutCleanup.resumeLogout(state, { stored = JSONObject(it.toString()) }, {
                cleanupCalls++
            }, {
                assertEquals("original", state.getString("generation"))
                providerCompleted = true
            }, {
                if (failAfterProvider) error("process interrupted")
                finished = true
                stored = null
            })
        }
        assertTrue(runCatching { run() }.isFailure)
        assertTrue(providerCompleted)
        assertTrue(stored!!.getBoolean("push_done"))
        assertEquals("original", stored!!.getString("generation"))
        failAfterProvider = false
        run()
        assertEquals(1, cleanupCalls)
        assertTrue(finished)
        assertNull(stored)
    }

    @Test fun noPushAndFailedProviderLogoutKeepGenerationForRetry() {
        var stored: JSONObject? = JSONObject().put("generation", "original")
        var providerCalls = 0
        fun run() {
            val state = JSONObject(stored!!.toString())
            PushLogoutCleanup.resumeLogout(state, { stored = JSONObject(it.toString()) }, {}, {
                providerCalls++
                if (providerCalls == 1) error("unknown outcome")
                assertEquals("original", state.getString("generation"))
            }, { stored = null })
        }
        assertTrue(runCatching { run() }.isFailure)
        assertTrue(stored!!.getBoolean("push_done"))
        run()
        assertEquals(2, providerCalls)
        assertNull(stored)
    }
}
