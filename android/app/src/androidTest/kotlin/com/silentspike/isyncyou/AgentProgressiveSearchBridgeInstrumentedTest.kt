package com.silentspike.isyncyou

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class AgentProgressiveSearchBridgeInstrumentedTest {
    @Test
    fun android_bridge_forwards_only_bounded_public_progress_projection() {
        val progress = validProgress()
        val encoded = BridgeMessagePolicy.outboundStreamEventJson("stream", progress.toString())
        assertTrue(encoded != null)
        val envelope = JSONObject(encoded!!)
        assertEquals("evt", envelope.getString("t"))
        assertEquals("stream", envelope.getString("id"))
        assertEquals(progress.toString(), envelope.getJSONObject("ev").toString())

        progress.put("schema_version", 2)
        assertNull(BridgeMessagePolicy.outboundStreamEventJson("stream", progress.toString()))
        assertNull(
            BridgeMessagePolicy.outboundStreamEventJson(
                "stream",
                JSONObject().put("event", "search_stage").toString(),
            ),
        )
    }

    @Test
    fun android_outbound_stream_wrapper_accepts_exact_72k_production_message() {
        val emptyEvent = JSONObject().put("event", "token").put("text", "")
        val emptyEnvelope = JSONObject()
            .put("t", "evt")
            .put("id", "stream")
            .put("ev", emptyEvent)
            .toString()
        val paddingBytes = BridgeMessagePolicy.MAX_OUTBOUND_STREAM_MESSAGE_BYTES -
            emptyEnvelope.toByteArray(Charsets.UTF_8).size
        val exactEvent = JSONObject()
            .put("event", "token")
            .put("text", "x".repeat(paddingBytes))
            .toString()
        val encoded = BridgeMessagePolicy.outboundStreamEventJson("stream", exactEvent)
        assertEquals(
            BridgeMessagePolicy.MAX_OUTBOUND_STREAM_MESSAGE_BYTES,
            encoded!!.toByteArray(Charsets.UTF_8).size,
        )
    }

    @Test
    fun android_outbound_stream_wrapper_rejects_one_over_without_partial_dispatch() {
        val emptyEvent = JSONObject().put("event", "token").put("text", "")
        val emptyEnvelope = JSONObject()
            .put("t", "evt")
            .put("id", "stream")
            .put("ev", emptyEvent)
            .toString()
        val paddingBytes = BridgeMessagePolicy.MAX_OUTBOUND_STREAM_MESSAGE_BYTES -
            emptyEnvelope.toByteArray(Charsets.UTF_8).size
        val oneOverEvent = JSONObject()
            .put("event", "token")
            .put("text", "x".repeat(paddingBytes + 1))
            .toString()
        assertNull(BridgeMessagePolicy.outboundStreamEventJson("stream", oneOverEvent))
    }

    @Test
    fun android_outbound_limit_includes_id_wrapper_and_json_escaping() {
        val id = "stream-\"-\\-" + "i".repeat(112)
        val event = JSONObject()
            .put("event", "token")
            .put("text", "\"\\\n")
            .toString()
        val encoded = BridgeMessagePolicy.outboundStreamEventJson(id, event)
        assertTrue(encoded != null)
        val envelope = JSONObject(encoded!!)
        assertEquals(id, envelope.getString("id"))
        assertEquals(event, envelope.getJSONObject("ev").toString())
        assertTrue(
            encoded.toByteArray(Charsets.UTF_8).size >
                event.toByteArray(Charsets.UTF_8).size + id.toByteArray(Charsets.UTF_8).size,
        )
    }

    private fun validProgress(): JSONObject = JSONObject()
        .put("event", "stage_progress")
        .put("schema_version", 1)
        .put("activity_id", "AAAAAAAAAAAAAAAAAAAAAA")
        .put("activity_kind", "archive_search")
        .put("stage", "names")
        .put("status", "running")
        .put("scanned", 0)
        .put("total", JSONObject.NULL)
        .put("hits", 0)
        .put("current_item", JSONObject.NULL)
        .put("coverage_complete", JSONObject.NULL)
        .put("budget_reached", JSONObject.NULL)
        .put("continuation_available", JSONObject.NULL)
}
