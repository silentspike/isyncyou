package com.silentspike.isyncyou

import java.util.UUID

data class OAuthGuardBeginResult(
    val guardId: String?,
    val error: String? = null,
) {
    val ok: Boolean
        get() = guardId != null
}

class OAuthGuardRegistry(
    private val onStart: () -> Unit,
    private val onStop: () -> Unit,
    private val newId: () -> String = { UUID.randomUUID().toString() },
) {
    private val active = LinkedHashSet<String>()

    @Synchronized
    fun begin(): OAuthGuardBeginResult {
        val wasEmpty = active.isEmpty()
        val id = newId()
        active.add(id)
        if (wasEmpty) {
            try {
                onStart()
            } catch (ex: RuntimeException) {
                active.remove(id)
                return OAuthGuardBeginResult(null, ex.javaClass.simpleName ?: "start_failed")
            }
        }
        return OAuthGuardBeginResult(id)
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
