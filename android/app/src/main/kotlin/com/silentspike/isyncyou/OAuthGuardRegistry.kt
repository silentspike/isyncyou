package com.silentspike.isyncyou

import java.util.UUID

class OAuthGuardRegistry(
    private val onStart: () -> Unit,
    private val onStop: () -> Unit,
    private val newId: () -> String = { UUID.randomUUID().toString() },
) {
    private val active = LinkedHashSet<String>()

    @Synchronized
    fun begin(): String {
        val wasEmpty = active.isEmpty()
        val id = newId()
        active.add(id)
        if (wasEmpty) onStart()
        return id
    }

    @Synchronized
    fun end(id: String?): Boolean {
        if (id.isNullOrBlank()) return false
        val removed = active.remove(id)
        if (removed && active.isEmpty()) onStop()
        return removed
    }

    @Synchronized
    fun activeCount(): Int = active.size

    @Synchronized
    fun clear(): Int {
        val count = active.size
        if (count > 0) {
            active.clear()
            onStop()
        }
        return count
    }
}
