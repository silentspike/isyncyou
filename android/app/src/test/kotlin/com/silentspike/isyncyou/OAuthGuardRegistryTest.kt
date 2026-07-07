package com.silentspike.isyncyou

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
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
        assertEquals("guard-a", first)
        assertEquals(1, startCalls)
        assertEquals(0, stopCalls)
        assertEquals(1, registry.activeCount())

        val second = registry.begin()
        assertEquals("guard-b", second)
        assertEquals(1, startCalls)
        assertEquals(0, stopCalls)
        assertEquals(2, registry.activeCount())

        assertTrue(registry.end(first))
        assertEquals(1, startCalls)
        assertEquals(0, stopCalls)
        assertEquals(1, registry.activeCount())

        assertTrue(registry.end(second))
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

        val id = registry.begin()
        assertFalse(registry.end("missing"))
        assertEquals(0, stopCalls)

        assertTrue(registry.end(id))
        assertEquals(1, stopCalls)
        assertFalse(registry.end(id))
        assertEquals(1, stopCalls)
    }
}
