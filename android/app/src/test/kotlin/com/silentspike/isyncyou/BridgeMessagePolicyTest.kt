package com.silentspike.isyncyou

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class BridgeMessagePolicyTest {
    @Test
    fun envelopeRejectsOversizedMalformedUnknownAndMissingIdMessages() {
        val oversized = "x".repeat(BridgeMessagePolicy.MAX_MESSAGE_BYTES + 1)
        assertInvalid(BridgeMessagePolicy.validateEnvelope(oversized), "too_large")
        assertInvalid(BridgeMessagePolicy.validateEnvelope("{"), "malformed_json")
        assertInvalid(
            BridgeMessagePolicy.validateEnvelope(JSONObject().put("t", "weird").put("id", "a").toString()),
            "unknown_type",
        )
        assertInvalid(
            BridgeMessagePolicy.validateEnvelope(JSONObject().put("t", "req").put("method", "GET").put("path", "/").toString()),
            "missing_id",
        )
    }

    @Test
    fun envelopeValidatesAllBridgeMessageTypes() {
        assertValid(
            BridgeMessagePolicy.validateEnvelope(
                JSONObject().put("t", "req").put("id", "r1").put("method", "GET").put("path", "/api/v1/items").toString(),
            ),
            "req",
            "r1",
        )
        assertInvalid(
            BridgeMessagePolicy.validateEnvelope(JSONObject().put("t", "req").put("id", "r2").put("method", "GET").toString()),
            "missing_path",
        )
        assertValid(
            BridgeMessagePolicy.validateEnvelope(JSONObject().put("t", "sub").put("id", "s1").put("path", "/events").toString()),
            "sub",
            "s1",
        )
        assertValid(
            BridgeMessagePolicy.validateEnvelope(JSONObject().put("t", "unsub").put("id", "u1").toString()),
            "unsub",
            "u1",
        )
        assertValid(
            BridgeMessagePolicy.validateEnvelope(
                JSONObject().put("t", "bio").put("id", "b1").put("pat", "pending-1").toString(),
            ),
            "bio",
            "b1",
        )
        assertInvalid(
            BridgeMessagePolicy.validateEnvelope(JSONObject().put("t", "bio").put("id", "b2").toString()),
            "missing_pat",
        )
    }

    @Test
    fun envelopeRejectsDuplicateTrailingUnknownAndWrongTypedFieldsBeforeSanitizing() {
        for (message in listOf(
            """{"t":"req","id":"r1","method":"GET","path":"/a","path":"/b"}""",
            """{"t":"req","id":"r2","method":"GET","path":"/a","headers":{"A":"1","A":"2"}}""",
            """{"t":"req","id":"r2b","method":"GET","path":"/a","headers":{"A":"1","\u0041":"2"}}""",
            """{"t":"req","id":"r3","method":"GET","path":"/a"} trailing""",
            """{"t":"req","id":"r4","method":"GET","path":"/a","unexpected":true}""",
            """{"t":"req","id":"r5","method":"GET","path":"/a","headers":{"Accept":7}}""",
            """{"t":"req","id":"r6","method":"GET","path":"/a","body":{}}""",
            """{"t":"req","id":"r7","method":"PUT","path":"/a"}""",
            """{"t":"req","id":"r8","method":"GET","path":"//a"}""",
            """{"t":"req","id":"r9","method":"GET","path":"/a#fragment"}""",
            """{"t":"req","id":"\ud800","method":"GET","path":"/a"}""",
        )) {
            assertFalse(
                "accepted raw envelope: $message",
                BridgeMessagePolicy.validateEnvelope(message).ok,
            )
        }
    }

    @Test
    fun requestEnvelopeRejectsAmbiguousOrLegacyHeaders() {
        for (headers in listOf(
            JSONObject().put("Content-Type", "application/json").put("content-type", "text/plain"),
            JSONObject().put("X-Body-Encoding", "base64"),
            JSONObject().put("Bad Header", "value"),
        )) {
            val message = JSONObject()
                .put("t", "req")
                .put("id", "request")
                .put("method", "POST")
                .put("path", "/api/v1/agent/oauth/start")
                .put("headers", headers)
                .put("body", "{}")
                .toString()
            assertInvalid(BridgeMessagePolicy.validateEnvelope(message), "invalid_headers")
        }
    }

    @Test
    fun nativeEnvelopeRejectsUnknownPayloadFieldsAndCoercedTypes() {
        assertInvalid(
            BridgeMessagePolicy.validateEnvelope(
                """{"t":"native","id":"n1","op":"beginNetworkGuard","payload":{"reason":"oauth","extra":true}}""",
            ),
            "missing_or_unknown_guard_reason",
        )
        assertInvalid(
            BridgeMessagePolicy.validateEnvelope(
                """{"t":"native","id":"n2","op":"endNetworkGuard","payload":{"guard_id":7}}""",
            ),
            "missing_guard_id",
        )
    }

    @Test
    fun nativeMessagesRequireTypedPayloadsAndKnownExternalKinds() {
        assertInvalid(
            BridgeMessagePolicy.validateEnvelope(JSONObject().put("t", "native").put("id", "n1").put("payload", JSONObject()).toString()),
            "missing_op",
        )
        assertInvalid(
            BridgeMessagePolicy.validateEnvelope(JSONObject().put("t", "native").put("id", "n2").put("op", "openExternal").toString()),
            "missing_payload",
        )
        assertInvalid(
            BridgeMessagePolicy.validateEnvelope(
                JSONObject()
                    .put("t", "native")
                    .put("id", "n3")
                    .put("op", "openExternal")
                    .put("payload", JSONObject().put("url", "https://auth.openai.com/oauth/authorize"))
                    .toString(),
            ),
            "missing_or_unknown_kind",
        )
        assertValid(
            BridgeMessagePolicy.validateEnvelope(
                JSONObject()
                    .put("t", "native")
                    .put("id", "n4")
                    .put("op", "openExternal")
                    .put(
                        "payload",
                        JSONObject()
                            .put("url", "https://auth.openai.com/oauth/authorize")
                            .put("kind", AuthUrlKind.AgentAuthorize.wire),
                    )
                    .toString(),
            ),
            "native",
            "n4",
        )
        assertInvalid(
            BridgeMessagePolicy.validateEnvelope(
                JSONObject()
                    .put("t", "native")
                    .put("id", "n5")
                    .put("op", "endNetworkGuard")
                    .put("payload", JSONObject())
                    .toString(),
            ),
            "missing_guard_id",
        )
        assertInvalid(
            BridgeMessagePolicy.validateEnvelope(
                JSONObject()
                    .put("t", "native")
                    .put("id", "n6")
                    .put("op", "beginNetworkGuard")
                    .put("payload", JSONObject())
                    .toString(),
            ),
            "missing_or_unknown_guard_reason",
        )
        assertValid(
            BridgeMessagePolicy.validateEnvelope(
                JSONObject()
                    .put("t", "native")
                    .put("id", "n7")
                    .put("op", "beginNetworkGuard")
                    .put("payload", JSONObject().put("reason", "oauth"))
                    .toString(),
            ),
            "native",
            "n7",
        )
        assertInvalid(
            BridgeMessagePolicy.validateEnvelope(
                JSONObject()
                    .put("t", "native")
                    .put("id", "n8")
                    .put("op", "bindNetworkGuard")
                    .put("payload", JSONObject().put("guard_id", "g").put("turn", "bad turn"))
                    .toString(),
            ),
            "invalid_turn",
        )
        assertInvalid(
            BridgeMessagePolicy.validateEnvelope(
                JSONObject()
                    .put("t", "native")
                    .put("id", "n9")
                    .put("op", "openNetworkSettings")
                    .put("payload", JSONObject().put("hint", "arbitrary"))
                    .toString(),
            ),
            "missing_or_unknown_settings_hint",
        )
        assertInvalid(
            BridgeMessagePolicy.validateEnvelope(
                JSONObject()
                    .put("t", "native")
                    .put("id", "n10")
                    .put("op", "captureNetworkSnapshot")
                    .put("payload", JSONObject())
                    .toString(),
            ),
            "missing_guard_id",
        )
        assertValid(
            BridgeMessagePolicy.validateEnvelope(
                JSONObject()
                    .put("t", "native")
                    .put("id", "n11")
                    .put("op", "captureNetworkSnapshot")
                    .put("payload", JSONObject().put("guard_id", "guard"))
                    .toString(),
            ),
            "native",
            "n11",
        )
        assertInvalid(
            BridgeMessagePolicy.validateEnvelope(
                JSONObject()
                    .put("t", "native")
                    .put("id", "n12")
                    .put("op", "arbitrary")
                    .put("payload", JSONObject())
                    .toString(),
            ),
            "unknown_native_op",
        )
    }

    @Test
    fun sanitizeHeadersOverridesSessionTokenCaseInsensitively() {
        val headers = JSONObject()
            .put("X-Session-Token", "attacker-1")
            .put("x-session-token", "attacker-2")
            .put("x-SeSsIoN-ToKeN", "attacker-3")
            .put("x-storage-not-low", "true")
            .put("Accept", "application/json")

        val sanitized = BridgeMessagePolicy.sanitizeHeaders(headers, "trusted", false)
        assertEquals("trusted", sanitized.getString("X-Session-Token"))
        assertEquals("application/json", sanitized.getString("Accept"))
        assertFalse(sanitized.has("x-session-token"))
        assertFalse(sanitized.has("x-SeSsIoN-ToKeN"))
        assertEquals("false", sanitized.getString("X-Storage-Not-Low"))
        assertEquals(3, sanitized.length())
    }

    @Test
    fun mutationIntentCreateChunkAndCommitRequireTrustedStorageState() {
        assertTrue(BridgeMessagePolicy.requiresTrustedStorageNotLow("/api/v1/mutation-intent/create"))
        assertTrue(BridgeMessagePolicy.requiresTrustedStorageNotLow("/api/v1/mutation-intent/chunk"))
        assertTrue(BridgeMessagePolicy.requiresTrustedStorageNotLow("/api/v1/mutation-intent/commit"))
        assertFalse(BridgeMessagePolicy.requiresTrustedStorageNotLow("/api/v1/mutation-intent/cancel"))
        assertFalse(BridgeMessagePolicy.requiresTrustedStorageNotLow("/api/v1/items"))
    }

    @Test
    fun android_bridge_ignores_cookie_authority_and_injects_native_session_header() {
        val headers = JSONObject()
            .put("Cookie", "isy_session=webview-controlled")
            .put("x-session-token", "webview-controlled")
            .put("X-Capability-Token", "agent-capability")

        val sanitized = BridgeMessagePolicy.sanitizeHeaders(headers, "native-session")

        assertEquals("native-session", sanitized.getString("X-Session-Token"))
        assertEquals("agent-capability", sanitized.getString("X-Capability-Token"))
        assertFalse(sanitized.has("x-session-token"))
        assertEquals("isy_session=webview-controlled", sanitized.getString("Cookie"))
    }

    @Test
    fun responseJsonEscapesUntrustedFields() {
        val id = "id\"}\n<script>"
        val body = JSONObject().put("error", "bad\"value").put("line", "\n")
        val reply = BridgeMessagePolicy.responseJson(id, 400, body)

        val parsed = JSONObject(reply)
        assertEquals("res", parsed.getString("t"))
        assertEquals(id, parsed.getString("id"))
        assertEquals(400, parsed.getInt("status"))

        val parsedBody = JSONObject(parsed.getString("body"))
        assertEquals("bad\"value", parsedBody.getString("error"))
        assertEquals("\n", parsedBody.getString("line"))
    }

    private fun assertValid(validation: BridgeValidation, type: String, id: String) {
        assertTrue(validation.ok)
        assertEquals(type, validation.type)
        assertEquals(id, validation.id)
    }

    private fun assertInvalid(validation: BridgeValidation, error: String) {
        assertFalse(validation.ok)
        assertEquals(error, validation.error)
    }
}
