package com.silentspike.isyncyou

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class OAuthGuardRegistryTest {
    @Test
    fun guardServiceIsReferenceCountedById() {
        var startCalls = 0
        var stopCalls = 0
        val ids = mutableListOf("guard-a", "guard-b")
        val registry = OAuthGuardRegistry(
            onStart = { startCalls += 1 },
            onStop = { stopCalls += 1 },
            newId = { ids.removeAt(0) },
        )

        val first = registry.begin()
        assertTrue(first.ok)
        assertEquals("guard-a", first.guardId)
        assertEquals(1, startCalls)
        assertEquals(0, stopCalls)
        assertEquals(1, registry.activeCount())

        val second = registry.begin()
        assertTrue(second.ok)
        assertEquals("guard-b", second.guardId)
        assertEquals(1, startCalls)
        assertEquals(0, stopCalls)
        assertEquals(2, registry.activeCount())

        assertTrue(registry.end(first.guardId))
        assertEquals(1, startCalls)
        assertEquals(0, stopCalls)
        assertEquals(1, registry.activeCount())

        assertTrue(registry.end(second.guardId))
        assertEquals(1, startCalls)
        assertEquals(1, stopCalls)
        assertEquals(0, registry.activeCount())
    }

    @Test
    fun endingUnknownOrBlankGuardDoesNotStopService() {
        var stopCalls = 0
        val registry = OAuthGuardRegistry(
            onStart = {},
            onStop = { stopCalls += 1 },
            newId = { "guard-a" },
        )

        assertFalse(registry.end(null))
        assertFalse(registry.end(""))
        assertFalse(registry.end("missing"))
        assertEquals(0, stopCalls)

        val id = registry.begin().guardId
        assertFalse(registry.end("missing"))
        assertEquals(0, stopCalls)

        assertTrue(registry.end(id))
        assertEquals(1, stopCalls)
        assertFalse(registry.end(id))
        assertEquals(1, stopCalls)
    }

    @Test
    fun clearStopsServiceOnceWhenGuardsAreActive() {
        var stopCalls = 0
        val ids = mutableListOf("guard-a", "guard-b")
        val registry = OAuthGuardRegistry(
            onStart = {},
            onStop = { stopCalls += 1 },
            newId = { ids.removeAt(0) },
        )

        registry.begin()
        registry.begin()

        assertEquals(2, registry.clear())
        assertEquals(1, stopCalls)
        assertEquals(0, registry.activeCount())
        assertEquals(0, registry.clear())
        assertEquals(1, stopCalls)
    }

    @Test
    fun failedStartRollsBackGuardAndCanRetry() {
        var startCalls = 0
        var failStart = true
        val ids = mutableListOf("guard-a", "guard-b")
        val registry = OAuthGuardRegistry(
            onStart = {
                startCalls += 1
                if (failStart) throw IllegalStateException("fgs denied")
            },
            onStop = {},
            newId = { ids.removeAt(0) },
        )

        val failed = registry.begin()
        assertFalse(failed.ok)
        assertNull(failed.guardId)
        assertEquals("IllegalStateException", failed.error)
        assertEquals(1, startCalls)
        assertEquals(0, registry.activeCount())

        failStart = false
        val retried = registry.begin()
        assertTrue(retried.ok)
        assertEquals("guard-b", retried.guardId)
        assertEquals(2, startCalls)
        assertEquals(1, registry.activeCount())
    }
}
