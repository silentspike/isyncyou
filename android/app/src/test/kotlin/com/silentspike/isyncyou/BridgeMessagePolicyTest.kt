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
    }

    @Test
    fun sanitizeHeadersOverridesSessionTokenCaseInsensitively() {
        val headers = JSONObject()
            .put("X-Session-Token", "attacker-1")
            .put("x-session-token", "attacker-2")
            .put("x-SeSsIoN-ToKeN", "attacker-3")
            .put("Accept", "application/json")

        val sanitized = BridgeMessagePolicy.sanitizeHeaders(headers, "trusted")
        assertEquals("trusted", sanitized.getString("X-Session-Token"))
        assertEquals("application/json", sanitized.getString("Accept"))
        assertFalse(sanitized.has("x-session-token"))
        assertFalse(sanitized.has("x-SeSsIoN-ToKeN"))
        assertEquals(2, sanitized.length())
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
