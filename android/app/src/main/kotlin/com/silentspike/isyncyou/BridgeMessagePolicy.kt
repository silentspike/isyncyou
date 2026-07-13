package com.silentspike.isyncyou

import org.json.JSONObject

data class BridgeValidation(
    val ok: Boolean,
    val type: String? = null,
    val id: String = "",
    val error: String? = null,
)

object BridgeMessagePolicy {
    const val MAX_MESSAGE_BYTES = 16 * 1024
    private const val MAX_ID_CHARS = 128
    private val TYPES = setOf("req", "sub", "unsub", "bio", "native")

    fun validateEnvelope(data: String): BridgeValidation {
        if (data.toByteArray(Charsets.UTF_8).size > MAX_MESSAGE_BYTES) {
            return BridgeValidation(false, error = "too_large")
        }
        val obj = try {
            JSONObject(data)
        } catch (_: Exception) {
            return BridgeValidation(false, error = "malformed_json")
        }
        val t = obj.optString("t", "")
        if (t !in TYPES) return BridgeValidation(false, id = obj.optString("id", ""), error = "unknown_type")
        val id = obj.optString("id", "")
        if (id.isBlank()) return BridgeValidation(false, type = t, error = "missing_id")
        if (id.length > MAX_ID_CHARS) return BridgeValidation(false, type = t, id = id, error = "id_too_long")
        return when (t) {
            "req" -> requireFields(obj, t, id, "method", "path")
            "sub" -> requireFields(obj, t, id, "path")
            "unsub" -> BridgeValidation(true, t, id)
            // The label is deliberately not accepted from WebView JS. Rust owns the
            // operation/service descriptor and Android maps those enums to fixed text.
            "bio" -> requireFields(obj, t, id, "pat")
            "native" -> validateNative(obj, t, id)
            else -> BridgeValidation(false, id = id, error = "unknown_type")
        }
    }

    fun sanitizeHeaders(headers: JSONObject?, trustedSessionToken: String): JSONObject {
        val out = JSONObject()
        if (headers != null) {
            val keys = headers.keys()
            while (keys.hasNext()) {
                val key = keys.next()
                if (key.lowercase() == "x-session-token") continue
                out.put(key, headers.opt(key))
            }
        }
        out.put("X-Session-Token", trustedSessionToken)
        return out
    }

    fun responseJson(id: String, status: Int, body: JSONObject): String =
        JSONObject()
            .put("t", "res")
            .put("id", id)
            .put("status", status)
            .put("body", body.toString())
            .toString()

    private fun requireFields(
        obj: JSONObject,
        type: String,
        id: String,
        vararg fields: String,
    ): BridgeValidation {
        for (field in fields) {
            if (obj.optString(field, "").isBlank()) {
                return BridgeValidation(false, type, id, "missing_$field")
            }
        }
        return BridgeValidation(true, type, id)
    }

    private fun validateNative(obj: JSONObject, type: String, id: String): BridgeValidation {
        val op = obj.optString("op", "")
        if (op.isBlank()) return BridgeValidation(false, type, id, "missing_op")
        val payload = obj.optJSONObject("payload")
            ?: return BridgeValidation(false, type, id, "missing_payload")
        if (op == "openExternal") {
            if (payload.optString("url", "").isBlank()) {
                return BridgeValidation(false, type, id, "missing_url")
            }
            if (ExternalUrlPolicy.authKindFromWire(payload.optString("kind", "")) == null) {
                return BridgeValidation(false, type, id, "missing_or_unknown_kind")
            }
        }
        when (op) {
            "beginNetworkGuard" -> {
                if (NetworkGuardReason.fromWire(payload.optString("reason", "")) == null) {
                    return BridgeValidation(false, type, id, "missing_or_unknown_guard_reason")
                }
            }
            "endNetworkGuard" -> {
                if (payload.optString("guard_id", "").isBlank()) {
                    return BridgeValidation(false, type, id, "missing_guard_id")
                }
            }
            "bindNetworkGuard" -> {
                if (payload.optString("guard_id", "").isBlank()) {
                    return BridgeValidation(false, type, id, "missing_guard_id")
                }
                if (!NetworkGuardPolicy.validTurnId(payload.optString("turn", ""))) {
                    return BridgeValidation(false, type, id, "invalid_turn")
                }
            }
            "captureNetworkSnapshot" -> {
                if (payload.optString("guard_id", "").isBlank()) {
                    return BridgeValidation(false, type, id, "missing_guard_id")
                }
            }
            "openNetworkSettings" -> {
                if (NetworkSettingsHint.fromWire(payload.optString("hint", "")) == null) {
                    return BridgeValidation(false, type, id, "missing_or_unknown_settings_hint")
                }
            }
            "openExternal", "pushToken" -> Unit
            else -> return BridgeValidation(false, type, id, "unknown_native_op")
        }
        return BridgeValidation(true, type, id)
    }
}
