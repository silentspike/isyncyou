package com.silentspike.isyncyou

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class NetworkCriticalGuardRegistryTest {
    @Test
    fun credentialRevokeGuardUsesTwoMinuteBoundedLease() {
        var now = 50L
        val registry = NetworkCriticalGuardRegistry(
            onStart = {},
            onStop = {},
            nowElapsedMs = { now },
            newId = { "revoke" },
        )

        assertTrue(registry.begin(NetworkGuardReason.CREDENTIAL_REVOKE).ok)
        assertEquals(now + 2 * 60_000L, registry.activeLease("revoke")?.deadlineElapsedMs)
        now += 2 * 60_000L
        assertNull(registry.activeLease("revoke"))
    }

    @Test
    fun credentialRevokeGuardSharesServiceWithoutEndingOtherReasons() {
        var starts = 0
        var stops = 0
        val ids = mutableListOf("oauth", "revoke")
        val registry = NetworkCriticalGuardRegistry(
            onStart = { starts += 1 },
            onStop = { stops += 1 },
            nowElapsedMs = { 100L },
            newId = { ids.removeAt(0) },
        )

        assertTrue(registry.begin(NetworkGuardReason.OAUTH).ok)
        assertTrue(registry.begin(NetworkGuardReason.CREDENTIAL_REVOKE).ok)
        assertEquals(1, starts)
        assertTrue(registry.end("revoke"))
        assertEquals(0, stops)
        assertEquals(NetworkGuardReason.OAUTH, registry.activeLease("oauth")?.reason)
        assertTrue(registry.end("oauth"))
        assertEquals(1, stops)
    }

    @Test
    fun credentialRevokeSnapshotRequiresExactLiveReadyGuard() {
        val registry = NetworkCriticalGuardRegistry(
            onStart = {},
            onStop = {},
            nowElapsedMs = { 1L },
            newId = { "revoke" },
        )

        assertNull(registry.activeLease("revoke"))
        assertTrue(registry.begin(NetworkGuardReason.CREDENTIAL_REVOKE).ok)
        assertEquals(
            NetworkGuardReason.CREDENTIAL_REVOKE,
            registry.activeLease("revoke")?.reason,
        )
        assertNull(registry.activeLease("other"))
        assertTrue(registry.end("revoke"))
        assertNull(registry.activeLease("revoke"))
    }

    @Test
    fun reasonsShareOneForegroundServiceUntilLastLeaseEnds() {
        var now = 1_000L
        var starts = 0
        var stops = 0
        val ids = mutableListOf("oauth", "turn")
        val registry = NetworkCriticalGuardRegistry(
            onStart = { starts += 1 },
            onStop = { stops += 1 },
            nowElapsedMs = { now },
            newId = { ids.removeAt(0) },
        )

        val oauth = registry.begin(NetworkGuardReason.OAUTH)
        val turn = registry.begin(NetworkGuardReason.AGENT_TURN)
        assertTrue(oauth.ok)
        assertTrue(turn.ok)
        assertEquals(1, starts)
        assertEquals(0, stops)
        assertEquals(2, registry.activeCount())

        assertTrue(registry.end(oauth.guardId))
        assertEquals(0, stops)
        assertTrue(registry.end(turn.guardId))
        assertEquals(1, stops)
        assertEquals(0, registry.activeCount())
        now += 1
    }

    @Test
    fun startFailureRollsBackOnlyNewLeaseAndCanRetry() {
        var now = 1L
        var fail = true
        var starts = 0
        val ids = mutableListOf("failed", "retry")
        val registry = NetworkCriticalGuardRegistry(
            onStart = {
                starts += 1
                if (fail) throw IllegalStateException("denied")
            },
            onStop = {},
            nowElapsedMs = { now },
            newId = { ids.removeAt(0) },
        )

        val first = registry.begin(NetworkGuardReason.OAUTH)
        assertFalse(first.ok)
        assertNull(first.guardId)
        assertEquals(0, registry.activeCount())
        fail = false
        val retry = registry.begin(NetworkGuardReason.OAUTH)
        assertTrue(retry.ok)
        assertEquals("retry", retry.guardId)
        assertEquals(2, starts)
        now += 1
    }

    @Test
    fun agentTurnBindingIsSingleUseAndExtendsOnlyStartingTurnLease() {
        var now = 10L
        val registry = NetworkCriticalGuardRegistry(
            onStart = {},
            onStop = {},
            nowElapsedMs = { now },
            newId = { "turn" },
        )
        val started = registry.begin(NetworkGuardReason.AGENT_TURN)
        val before = registry.snapshot().single().deadlineElapsedMs
        now += 500L
        assertTrue(registry.bindTurn(started.guardId, "turn-123"))
        val after = registry.snapshot().single()
        assertEquals("turn-123", after.turnId)
        assertTrue(after.deadlineElapsedMs > before)
        assertFalse(registry.bindTurn(started.guardId, "turn-124"))
        assertFalse(registry.bindTurn(started.guardId, "bad turn"))
    }

    @Test
    fun expiryRemovesOnlyExpiredLeaseAndKeepsOtherReasonAlive() {
        var now = 100L
        var stops = 0
        val ids = mutableListOf("refresh", "oauth")
        val registry = NetworkCriticalGuardRegistry(
            onStart = {},
            onStop = { stops += 1 },
            nowElapsedMs = { now },
            newId = { ids.removeAt(0) },
        )
        registry.begin(NetworkGuardReason.CREDENTIAL_REFRESH)
        registry.begin(NetworkGuardReason.OAUTH)
        now += NetworkGuardReason.CREDENTIAL_REFRESH.startingLeaseMs
        assertEquals(1, registry.reapExpired())
        assertEquals(1, registry.activeCount())
        assertEquals(0, stops)
        now += NetworkGuardReason.OAUTH.startingLeaseMs
        assertEquals(1, registry.reapExpired())
        assertEquals(1, stops)
    }

    @Test
    fun recreationDoesNotClearLeaseButServiceDestructionInvalidatesIt() {
        var now = 5L
        val registry = NetworkCriticalGuardRegistry(
            onStart = {},
            onStop = {},
            nowElapsedMs = { now },
            newId = { "oauth" },
        )
        assertNotNull(registry.begin(NetworkGuardReason.OAUTH).guardId)
        assertEquals(1, registry.activeCount())
        assertEquals(1, registry.invalidateAll())
        assertEquals(0, registry.activeCount())
        now += 1
    }

    @Test
    fun ninthLeaseIsRejectedWithoutChangingServiceState() {
        var now = 1L
        var starts = 0
        var next = 0
        val registry = NetworkCriticalGuardRegistry(
            onStart = { starts += 1 },
            onStop = {},
            nowElapsedMs = { now },
            newId = { "g-${next++}" },
        )
        repeat(NetworkGuardPolicy.MAX_LEASES) {
            assertTrue(registry.begin(NetworkGuardReason.OAUTH).ok)
        }
        val denied = registry.begin(NetworkGuardReason.OAUTH)
        assertFalse(denied.ok)
        assertEquals("lease_limit", denied.error)
        assertEquals(NetworkGuardPolicy.MAX_LEASES, registry.activeCount())
        assertEquals(1, starts)
        now += 1
    }

    @Test
    fun activeLeaseRejectsMissingExpiredAndUnknownGuards() {
        var now = 1L
        val registry = NetworkCriticalGuardRegistry(
            onStart = {},
            onStop = {},
            nowElapsedMs = { now },
            newId = { "oauth" },
        )
        assertNull(registry.activeLease("oauth"))
        assertTrue(registry.begin(NetworkGuardReason.OAUTH).ok)
        assertEquals(NetworkGuardReason.OAUTH, registry.activeLease("oauth")?.reason)
        now += NetworkGuardReason.OAUTH.startingLeaseMs
        assertNull(registry.activeLease("oauth"))
    }
}
