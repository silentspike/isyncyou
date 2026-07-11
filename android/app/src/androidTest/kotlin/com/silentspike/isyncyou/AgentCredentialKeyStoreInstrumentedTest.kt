package com.silentspike.isyncyou

import android.util.Log
import androidx.test.platform.app.InstrumentationRegistry
import java.io.File
import java.util.Arrays
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.fail
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeNoException
import org.junit.Test

class AgentCredentialKeyStoreInstrumentedTest {
    @Test
    fun corruptWrappedAgentCredentialKeyFailsClosedWithoutReplacement() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val root = File(context.cacheDir, "agent-credential-corruption-${System.nanoTime()}")
        assertTrue(root.mkdirs())
        try {
            val created = AgentCredentialKeyStore.getOrCreate(root)
            Arrays.fill(created.key, 0)
            val wrapped = File(root, AgentCredentialKeyStore.KEY_FILE)
            assertTrue(wrapped.isFile)
            val corrupt = "corrupt-wrapped-key".toByteArray()
            wrapped.writeBytes(corrupt)

            try {
                val unexpected = AgentCredentialKeyStore.getOrCreate(root)
                Arrays.fill(unexpected.key, 0)
                fail("corrupt wrapped key was silently accepted or replaced")
            } catch (_: Exception) {
                // Expected: corruption is terminal for this startup attempt.
            }
            assertTrue(wrapped.readBytes().contentEquals(corrupt))
        } finally {
            root.deleteRecursively()
        }
    }

    @Test
    fun agentCredentialKeyStoreInstallsAndSealsProviderCredentialStore() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val filesDir = context.filesDir
        File(filesDir, AgentCredentialKeyStore.DEBUG_FAIL_FILE).delete()
        val sentinel = "agent-credential-pixel-sentinel-${System.nanoTime()}"

        val result = AgentCredentialKeyStore.getOrCreate(filesDir)
        try {
            assertEquals(32, result.key.size)
            assertEquals(1, NativeEngine.nativeSetAgentCredentialKey(result.key))
        } finally {
            Arrays.fill(result.key, 0)
        }

        val selfTestJson = try {
            NativeEngine.nativeAgentCredentialStoreSelfTest(filesDir.absolutePath, sentinel)
        } catch (e: UnsatisfiedLinkError) {
            assumeNoException("agent-credential-store-self-test native feature is not enabled", e)
            return
        }

        assertFalse("self-test output must not include the sentinel", selfTestJson.contains(sentinel))
        val selfTest = JSONObject(selfTestJson)
        assertEquals("agent_credential_store", selfTest.getString("self_test"))
        assertEquals("jni_only_feature_gated", selfTest.getString("scope"))
        assertEquals("ok", selfTest.getString("status"))
        assertEquals("android_installed", selfTest.getString("key_source"))
        assertTrue(selfTest.getBoolean("round_trip"))
        assertFalse(selfTest.getBoolean("plaintext_sentinel_in_credential_store"))
        assertFalse(selfTest.getBoolean("plaintext_sentinel_in_wrapped_key_file"))
        assertEquals("agent-credentials", selfTest.getString("credential_store_dir"))
        assertEquals("agent_credential.key", selfTest.getString("wrapped_key_file"))

        val evidence = JSONObject()
            .put("self_test", "agent_credential_keystore")
            .put("alias", result.evidence.alias)
            .put("key_file", result.evidence.keyFile)
            .put("just_created", result.justCreated)
            .put("algorithm", result.evidence.algorithm ?: JSONObject.NULL)
            .put("key_size", result.evidence.keySize ?: JSONObject.NULL)
            .put(
                "inside_secure_hardware",
                result.evidence.insideSecureHardware ?: JSONObject.NULL,
            )
            .put("security_level", result.evidence.securityLevel ?: JSONObject.NULL)
            .put(
                "metadata_unavailable_reason",
                result.evidence.metadataUnavailableReason ?: JSONObject.NULL,
            )
        assertEquals("isyncyou-agent-credential-wrap-v1", evidence.getString("alias"))
        assertEquals("agent_credential.key", evidence.getString("key_file"))
        assertFalse(fileTreeContains(filesDir, sentinel.toByteArray()))

        Log.i("iSyncYouAgentCred", evidence.toString())
        Log.i("iSyncYouAgentCred", selfTestJson)
        println("ISY_AGENT_CREDENTIAL_KEYSTORE_JSON=$evidence")
        println("ISY_AGENT_CREDENTIAL_SELF_TEST_JSON=$selfTestJson")
    }

    private fun fileTreeContains(root: File, needle: ByteArray): Boolean {
        if (!root.exists()) return false
        val stack = ArrayDeque<File>()
        stack.add(root)
        while (!stack.isEmpty()) {
            val file = stack.removeLast()
            if (file.isDirectory) {
                file.listFiles()?.forEach { stack.add(it) }
            } else if (file.isFile) {
                val bytes = file.readBytes()
                if (containsBytes(bytes, needle)) {
                    return true
                }
            }
        }
        return false
    }

    private fun containsBytes(bytes: ByteArray, needle: ByteArray): Boolean {
        if (needle.isEmpty() || bytes.size < needle.size) return false
        for (offset in 0..(bytes.size - needle.size)) {
            var matches = true
            for (i in needle.indices) {
                if (bytes[offset + i] != needle[i]) {
                    matches = false
                    break
                }
            }
            if (matches) return true
        }
        return false
    }
}
