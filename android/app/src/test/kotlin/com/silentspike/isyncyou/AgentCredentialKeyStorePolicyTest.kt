package com.silentspike.isyncyou

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Test

class AgentCredentialKeyStorePolicyTest {
    @Test
    fun agentCredentialKeyStoreUsesSeparateAliasFileAndFailureFlag() {
        assertEquals("isyncyou-agent-credential-wrap-v1", AgentCredentialKeyStore.WRAP_ALIAS)
        assertEquals("agent_credential.key", AgentCredentialKeyStore.KEY_FILE)
        assertEquals(".debug-fail-agent-credential-key", AgentCredentialKeyStore.DEBUG_FAIL_FILE)

        assertNotEquals("isyncyou-body-wrap-v1", AgentCredentialKeyStore.WRAP_ALIAS)
        assertNotEquals("body.key", AgentCredentialKeyStore.KEY_FILE)
        assertNotEquals(".debug-fail-body-key", AgentCredentialKeyStore.DEBUG_FAIL_FILE)
    }
}
