package com.silentspike.isyncyou

/** Exact queue-producing HTTP success policy. Query strings are intentionally ignored. */
internal object MobileJobWakeupPolicy {
    private val paths = setOf(
        "/api/v1/backup",
        "/api/v1/restore",
        "/api/v1/agent/confirm",
    )

    fun shouldReconcile(method: String?, rawPath: String?, status: Int): Boolean {
        if (!method.equals("POST", ignoreCase = true) || status !in 200..299) return false
        val value = rawPath ?: return false
        if (!value.startsWith('/') || value.contains("://") || value.contains('#')) return false
        val path = value.substringBefore('?')
        return path in paths
    }

    fun shouldReconcileAfterEngineReady(sessionToken: String): Boolean = sessionToken.isNotBlank()
}
