package com.silentspike.isyncyou

import java.util.UUID

data class NetworkGuardBeginResult(
    val guardId: String?,
    val error: String? = null,
) {
    val ok: Boolean
        get() = guardId != null
}

data class NetworkGuardLeaseSnapshot(
    val id: String,
    val reason: NetworkGuardReason,
    val deadlineElapsedMs: Long,
    val turnId: String?,
)

/**
 * Process-owned lease registry for short foreground network work. It has no Android
 * dependencies so lifecycle and deadline behavior remain deterministic in JVM tests.
 */
class NetworkCriticalGuardRegistry(
    private val onStart: () -> Unit,
    private val onStop: () -> Unit,
    private val nowElapsedMs: () -> Long,
    private val newId: () -> String = { UUID.randomUUID().toString() },
    private val maxLeases: Int = NetworkGuardPolicy.MAX_LEASES,
) {
    private data class Lease(
        val reason: NetworkGuardReason,
        var deadlineElapsedMs: Long,
        var turnId: String? = null,
    )

    private val active = LinkedHashMap<String, Lease>()

    @Synchronized
    fun begin(reason: NetworkGuardReason): NetworkGuardBeginResult {
        reapExpiredLocked(nowElapsedMs())
        if (active.size >= maxLeases) return NetworkGuardBeginResult(null, "lease_limit")
        val id = newId()
        if (active.containsKey(id)) return NetworkGuardBeginResult(null, "id_collision")
        val wasEmpty = active.isEmpty()
        active[id] = Lease(reason, addBounded(nowElapsedMs(), reason.startingLeaseMs))
        if (wasEmpty) {
            try {
                onStart()
            } catch (ex: RuntimeException) {
                active.remove(id)
                return NetworkGuardBeginResult(null, ex.javaClass.simpleName ?: "start_failed")
            }
        }
        return NetworkGuardBeginResult(id)
    }

    @Synchronized
    fun bindTurn(id: String?, turnId: String?): Boolean {
        if (id.isNullOrBlank() || turnId.isNullOrBlank() || !NetworkGuardPolicy.validTurnId(turnId)) {
            return false
        }
        reapExpiredLocked(nowElapsedMs())
        val lease = active[id] ?: return false
        if (lease.reason != NetworkGuardReason.AGENT_TURN || lease.turnId != null) return false
        lease.turnId = turnId
        lease.deadlineElapsedMs = addBounded(nowElapsedMs(), lease.reason.activeLeaseMs)
        return true
    }

    @Synchronized
    fun end(id: String?): Boolean {
        if (id.isNullOrBlank()) return false
        reapExpiredLocked(nowElapsedMs())
        val removed = active.remove(id) ?: return false
        if (active.isEmpty()) onStop()
        return true
    }

    @Synchronized
    fun reapExpired(): Int = reapExpiredLocked(nowElapsedMs())

    @Synchronized
    fun invalidateAll(): Int {
        val count = active.size
        active.clear()
        return count
    }

    @Synchronized
    fun activeCount(): Int = active.size

    @Synchronized
    fun nextDeadlineElapsedMs(): Long? = active.values.minOfOrNull { it.deadlineElapsedMs }

    @Synchronized
    fun snapshot(): List<NetworkGuardLeaseSnapshot> = active.map { (id, lease) ->
        NetworkGuardLeaseSnapshot(id, lease.reason, lease.deadlineElapsedMs, lease.turnId)
    }

    private fun reapExpiredLocked(now: Long): Int {
        val expired = active.filterValues { it.deadlineElapsedMs <= now }.keys.toList()
        expired.forEach(active::remove)
        if (expired.isNotEmpty() && active.isEmpty()) onStop()
        return expired.size
    }

    private fun addBounded(left: Long, right: Long): Long =
        if (left > Long.MAX_VALUE - right) Long.MAX_VALUE else left + right
}
