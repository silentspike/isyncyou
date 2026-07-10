package com.silentspike.isyncyou

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class EngineBootstrapPolicyTest {
    @Test
    fun startupInstallsBodyThenAgentKeyBeforeNativeStartAndWipesBoth() {
        val events = mutableListOf<String>()
        val body = ByteArray(32) { 1 }
        val agent = ByteArray(32) { 2 }
        val result = EngineBootstrap.runNativeStartupSequence(
            1,
            body,
            agent,
            { id, key -> events += "body:$id:${key[0]}"; true },
            { key -> events += "agent:${key[0]}"; true },
            { events += "start"; 7 },
        )
        assertEquals(7, result)
        assertEquals(listOf("body:1:1", "agent:2", "start"), events)
        assertTrue(body.all { it == 0.toByte() })
        assertTrue(agent.all { it == 0.toByte() })
    }

    @Test
    fun bodyKeyFailureDoesNotCallAgentInstallOrNativeStart() {
        val events = mutableListOf<String>()
        val body = ByteArray(32) { 1 }
        val agent = ByteArray(32) { 2 }
        try {
            EngineBootstrap.runNativeStartupSequence(
                1,
                body,
                agent,
                { _, _ -> events += "body"; false },
                { events += "agent"; true },
                { events += "start"; 7 },
            )
        } catch (e: EncryptedStorageSetupException) {
            assertEquals("Encrypted storage key install failed", e.message)
        }
        assertEquals(listOf("body"), events)
        assertTrue(body.all { it == 0.toByte() })
        assertTrue(agent.all { it == 0.toByte() })
    }

    @Test
    fun agentKeyFailureDoesNotCallNativeStart() {
        val events = mutableListOf<String>()
        val body = ByteArray(32) { 1 }
        val agent = ByteArray(32) { 2 }
        try {
            EngineBootstrap.runNativeStartupSequence(
                1,
                body,
                agent,
                { _, _ -> events += "body"; true },
                { events += "agent"; false },
                { events += "start"; 7 },
            )
        } catch (e: EncryptedStorageSetupException) {
            assertEquals("Agent credential storage key install failed", e.message)
        }
        assertEquals(listOf("body", "agent"), events)
        assertTrue(body.all { it == 0.toByte() })
        assertTrue(agent.all { it == 0.toByte() })
    }
}
