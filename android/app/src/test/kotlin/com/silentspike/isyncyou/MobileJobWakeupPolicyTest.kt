package com.silentspike.isyncyou

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class MobileJobWakeupPolicyTest {
    @Test
    fun accepts_only_exact_post_paths_and_success_status() {
        assertTrue(MobileJobWakeupPolicy.shouldReconcile("POST", "/api/v1/backup?account=me", 202))
        assertTrue(MobileJobWakeupPolicy.shouldReconcile("post", "/api/v1/restore?account=me", 200))
        assertTrue(MobileJobWakeupPolicy.shouldReconcile("POST", "/api/v1/agent/confirm?pending=x", 204))
        assertFalse(MobileJobWakeupPolicy.shouldReconcile("GET", "/api/v1/backup", 200))
        assertFalse(MobileJobWakeupPolicy.shouldReconcile("POST", "/api/v1/backupish", 200))
        assertFalse(MobileJobWakeupPolicy.shouldReconcile("POST", "/api/v1/backup", 500))
    }

    @Test
    fun malformed_or_non_api_paths_do_not_reconcile() {
        assertFalse(MobileJobWakeupPolicy.shouldReconcile("POST", null, 200))
        assertFalse(MobileJobWakeupPolicy.shouldReconcile("POST", "https://evil.example/api/v1/backup", 200))
    }
}
