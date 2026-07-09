package com.silentspike.isyncyou

import android.util.Log
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeNoException
import org.junit.Test

class AgentSessionKdfBenchmarkInstrumentedTest {
    @Test
    fun agentSessionKdfBenchmarkProducesEvidenceJson() {
        val json = try {
            NativeEngine.nativeAgentSessionKdfBenchmark(5)
        } catch (e: UnsatisfiedLinkError) {
            assumeNoException("agent-session-kdf-bench native feature is not enabled", e)
            return
        }
        Log.i("iSyncYouKdfBench", json)
        println("ISY_KDF_BENCH_JSON=$json")
        val parsed = JSONObject(json)
        assertEquals("agent_session_argon2id_hkdf", parsed.getString("benchmark"))
        assertEquals("jni_only_feature_gated", parsed.getString("scope"))
        assertEquals(5, parsed.getInt("iterations"))
        assertTrue(parsed.getDouble("median_ms") > 0.0)
        assertTrue(parsed.getDouble("p95_ms") >= parsed.getDouble("median_ms"))
        val kdf = parsed.getJSONObject("kdf")
        assertEquals("argon2id-hkdf-sha256", kdf.getString("alg"))
        assertEquals(65_536, kdf.getInt("memory_kib"))
        assertEquals(3, kdf.getInt("iterations"))
        assertEquals(4, kdf.getInt("lanes"))
    }
}
