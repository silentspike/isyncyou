package com.silentspike.isyncyou

import org.json.JSONArray
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
    fun android_outbound_stream_wrapper_accepts_maximum_valid_partial_result() {
        val event = maximumValidPartialResult()
        val encoded = BridgeMessagePolicy.outboundStreamEventJson("stream", event.toString())
        assertTrue(encoded != null)
        assertTrue(
            encoded!!.toByteArray(Charsets.UTF_8).size <=
                BridgeMessagePolicy.MAX_OUTBOUND_STREAM_MESSAGE_BYTES,
        )
        assertEquals(20, JSONObject(encoded).getJSONObject("ev").getJSONArray("items").length())
    }

    @Test
    fun android_outbound_stream_wrapper_rejects_partial_result_one_over_item_limit() {
        val event = maximumValidPartialResult()
        event.getJSONArray("items").put(event.getJSONArray("items").getJSONObject(0))
        assertNull(BridgeMessagePolicy.outboundStreamEventJson("stream", event.toString()))
    }

    @Test
    fun android_public_progress_rejects_body_excerpt_member() {
        val event = maximumValidPartialResult()
        event.getJSONArray("items").getJSONObject(0).put("snippet", "private body")
        assertNull(BridgeMessagePolicy.outboundStreamEventJson("stream", event.toString()))
    }

    @Test
    fun android_outbound_stream_wrapper_rejects_message_one_byte_over_limit() {
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

    @Test
    fun android_progress_accepts_failed_then_skipped_terminal_chain() {
        val stages = listOf(
            stage("names", "queued"),
            stage("bodies", "queued"),
            stage("deep", "queued"),
            stage("names", "running"),
            stage("names", "failed"),
            stage("bodies", "skipped"),
            stage("deep", "skipped"),
        )
        stages.forEach { event ->
            assertTrue(BridgeMessagePolicy.outboundStreamEventJson("stream", event.toString()) != null)
        }
    }

    private fun stage(name: String, status: String): JSONObject = validProgress()
        .put("stage", name)
        .put("status", status)

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

    private fun maximumValidPartialResult(): JSONObject {
        val items = JSONArray()
        repeat(20) { index ->
            val name = "n".repeat(192)
            val itemId = "i".repeat(509) + "%03d".format(index)
            items.put(
                JSONObject()
                    .put("result_key", "A".repeat(19) + "%03d".format(index))
                    .put("change", "add")
                    .put("service", "mail")
                    .put("item_id", itemId)
                    .put("name", name)
                    .put("item_type", "m".repeat(64))
                    .put("display_path", "p".repeat(768))
                    .put("sender", "s".repeat(256))
                    .put("body_available", true)
                    .put(
                        "source",
                        JSONObject()
                            .put("service", "mail")
                            .put("item_id", itemId)
                            .put("label", name),
                    ),
            )
        }
        return JSONObject()
            .put("event", "partial_result")
            .put("schema_version", 1)
            .put("activity_id", "A".repeat(22))
            .put("stage", "deep")
            .put("sequence", 0)
            .put("items", items)
    }
}
