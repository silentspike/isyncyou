package com.silentspike.isyncyou

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ProviderCatchUpPolicyTest {
    @Test fun requiresEveryRequestedAccountToFinish() {
        val accounts = listOf("one", "two")
        assertTrue(ProviderCatchUpPolicy.complete(accounts, mapOf("one" to true, "two" to true)))
        assertFalse(ProviderCatchUpPolicy.complete(accounts, mapOf("one" to true, "two" to false)))
        assertFalse(ProviderCatchUpPolicy.complete(accounts, mapOf("one" to true)))
    }
}
