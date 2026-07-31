package com.silentspike.isyncyou

import org.json.JSONArray
import org.json.JSONObject

data class BridgeValidation(
    val ok: Boolean,
    val type: String? = null,
    val id: String = "",
    val error: String? = null,
)

object BridgeMessagePolicy {
    const val MAX_MESSAGE_BYTES = 16 * 1024
    const val MAX_OUTBOUND_STREAM_MESSAGE_BYTES = 72 * 1024
    private const val MAX_ID_CHARS = 128
    private const val MAX_PATH_CHARS = 8 * 1024
    private const val MAX_HEADER_VALUE_CHARS = 8 * 1024
    private const val MAX_STAGE_PROGRESS_BYTES = 4 * 1024
    private const val MAX_PARTIAL_RESULT_BYTES = 64 * 1024
    private const val MAX_PUBLIC_COUNTER = 1_000_000L
    private const val MAX_PARTIAL_ITEMS = 20
    private val TYPES = setOf("req", "sub", "unsub", "bio", "native")
    private val HEADER_NAME = Regex("^[A-Za-z0-9-]{1,128}$")
    private val OPAQUE_ACTIVITY_ID = Regex("^[A-Za-z0-9_-]{22}$")
    private val PROGRESS_STAGES = setOf("names", "bodies", "deep")
    private val PROGRESS_STATUSES = setOf(
        "queued",
        "running",
        "complete",
        "failed",
        "skipped",
        "cancelled",
    )
    private val RESULT_CHANGES = setOf("add", "enrich")
    private val RESULT_SERVICES = setOf(
        "mail",
        "onedrive",
        "calendar",
        "contacts",
        "todo",
        "onenote",
    )
    private val STORAGE_GUARDED_PATHS = setOf(
        "/api/v1/mutation-intent/create",
        "/api/v1/mutation-intent/chunk",
        "/api/v1/mutation-intent/commit",
    )

    fun requiresTrustedStorageNotLow(path: String): Boolean = path in STORAGE_GUARDED_PATHS

    fun validateEnvelope(data: String): BridgeValidation {
        if (data.toByteArray(Charsets.UTF_8).size > MAX_MESSAGE_BYTES) {
            return BridgeValidation(false, error = "too_large")
        }
        if (!StrictJsonSyntax.validateObject(data)) {
            return BridgeValidation(false, error = "malformed_json")
        }
        val obj = try {
            JSONObject(data)
        } catch (_: Exception) {
            return BridgeValidation(false, error = "malformed_json")
        }
        val t = exactString(obj, "t") ?: return BridgeValidation(false, error = "unknown_type")
        val id = exactString(obj, "id") ?: ""
        if (t !in TYPES) return BridgeValidation(false, id = id, error = "unknown_type")
        if (id.isBlank()) return BridgeValidation(false, type = t, error = "missing_id")
        if (id.length > MAX_ID_CHARS) return BridgeValidation(false, type = t, id = id, error = "id_too_long")
        return when (t) {
            "req" -> validateRequest(obj, t, id)
            "sub" -> validateSubscription(obj, t, id)
            "unsub" -> if (hasOnlyKeys(obj, setOf("t", "id"))) {
                BridgeValidation(true, t, id)
            } else {
                BridgeValidation(false, t, id, "unknown_field")
            }
            // The label is deliberately not accepted from WebView JS. Rust owns the
            // operation/service descriptor and Android maps those enums to fixed text.
            "bio" -> if (
                hasOnlyKeys(obj, setOf("t", "id", "pat")) &&
                !exactString(obj, "pat").isNullOrBlank()
            ) {
                BridgeValidation(true, t, id)
            } else {
                BridgeValidation(false, t, id, "missing_pat")
            }
            "native" -> validateNative(obj, t, id)
            else -> BridgeValidation(false, id = id, error = "unknown_type")
        }
    }

    fun sanitizeHeaders(
        headers: JSONObject?,
        trustedSessionToken: String,
        trustedStorageNotLow: Boolean? = null,
    ): JSONObject {
        val out = JSONObject()
        if (headers != null) {
            val keys = headers.keys()
            while (keys.hasNext()) {
                val key = keys.next()
                if (key.lowercase() in setOf("x-session-token", "x-storage-not-low")) continue
                out.put(key, headers.opt(key))
            }
        }
        out.put("X-Session-Token", trustedSessionToken)
        if (trustedStorageNotLow != null) {
            out.put("X-Storage-Not-Low", trustedStorageNotLow.toString())
        }
        return out
    }

    fun responseJson(id: String, status: Int, body: JSONObject): String =
        JSONObject()
            .put("t", "res")
            .put("id", id)
            .put("status", status)
            .put("body", body.toString())
            .toString()

    fun outboundStreamEventJson(id: String, eventJson: String): String? {
        if (id.isBlank() || id.length > MAX_ID_CHARS || !StrictJsonSyntax.validateObject(eventJson)) {
            return null
        }
        val event = try {
            JSONObject(eventJson)
        } catch (_: Exception) {
            return null
        }
        val eventType = exactString(event, "event") ?: return null
        if (eventType == "search_stage") return null
        if (
            eventType in setOf("stage_progress", "partial_result") &&
            !validateProgressEvent(eventType, event, eventJson)
        ) {
            return null
        }
        val envelope = JSONObject()
            .put("t", "evt")
            .put("id", id)
            .put("ev", event)
            .toString()
        return envelope.takeIf {
            it.toByteArray(Charsets.UTF_8).size <= MAX_OUTBOUND_STREAM_MESSAGE_BYTES
        }
    }

    private fun validateProgressEvent(
        eventType: String,
        event: JSONObject,
        eventJson: String,
    ): Boolean {
        if (exactLong(event, "schema_version") != 1L) return false
        val activityId = exactString(event, "activity_id") ?: return false
        if (!OPAQUE_ACTIVITY_ID.matches(activityId)) return false
        return when (eventType) {
            "stage_progress" -> {
                val allowed = setOf(
                    "event",
                    "schema_version",
                    "activity_id",
                    "activity_kind",
                    "stage",
                    "status",
                    "scanned",
                    "total",
                    "hits",
                    "current_item",
                    "coverage_complete",
                    "budget_reached",
                    "continuation_available",
                )
                hasExactKeys(event, allowed) &&
                    exactString(event, "activity_kind") == "archive_search" &&
                    exactString(event, "stage") in PROGRESS_STAGES &&
                    exactString(event, "status") in PROGRESS_STATUSES &&
                    boundedCounter(event, "scanned") &&
                    nullableBoundedCounter(event, "total") &&
                    boundedCounter(event, "hits") &&
                    nullableBoundedString(event, "current_item", 160) &&
                    nullableBoolean(event, "coverage_complete") &&
                    nullableBoolean(event, "budget_reached") &&
                    nullableBoolean(event, "continuation_available") &&
                    eventJson.toByteArray(Charsets.UTF_8).size <= MAX_STAGE_PROGRESS_BYTES
            }
            "partial_result" -> {
                if (
                    !hasExactKeys(
                        event,
                        setOf(
                            "event",
                            "schema_version",
                            "activity_id",
                            "stage",
                            "sequence",
                            "items",
                        ),
                    ) ||
                    exactString(event, "stage") !in PROGRESS_STAGES ||
                    exactLong(event, "sequence") !in 0L..65535L ||
                    eventJson.toByteArray(Charsets.UTF_8).size > MAX_PARTIAL_RESULT_BYTES
                ) {
                    return false
                }
                val items = event.opt("items") as? JSONArray ?: return false
                if (items.length() > MAX_PARTIAL_ITEMS) return false
                (0 until items.length()).all { index ->
                    (items.opt(index) as? JSONObject)?.let(::validatePublicSearchItem) == true
                }
            }
            else -> false
        }
    }

    private fun validatePublicSearchItem(item: JSONObject): Boolean {
        if (
            !hasExactKeys(
                item,
                setOf(
                    "result_key",
                    "change",
                    "service",
                    "item_id",
                    "name",
                    "item_type",
                    "display_path",
                    "sender",
                    "body_available",
                    "source",
                ),
            )
        ) {
            return false
        }
        val resultKey = exactString(item, "result_key") ?: return false
        val service = exactString(item, "service") ?: return false
        val itemId = exactString(item, "item_id") ?: return false
        val name = exactString(item, "name") ?: return false
        val itemType = exactString(item, "item_type") ?: return false
        if (
            !OPAQUE_ACTIVITY_ID.matches(resultKey) ||
            exactString(item, "change") !in RESULT_CHANGES ||
            service !in RESULT_SERVICES ||
            !boundedRequiredString(itemId, 512) ||
            !boundedRequiredString(name, 192) ||
            !boundedRequiredString(itemType, 64) ||
            !nullableBoundedString(item, "display_path", 768) ||
            !nullableBoundedString(item, "sender", 256) ||
            item.opt("body_available") !is Boolean
        ) {
            return false
        }
        val source = item.opt("source") as? JSONObject ?: return false
        if (!hasExactKeys(source, setOf("service", "item_id", "label"))) return false
        return exactString(source, "service") == service &&
            exactString(source, "item_id") == itemId &&
            nullableBoundedString(source, "label", 192) &&
            (source.isNull("label") || exactString(source, "label") == name)
    }

    private fun exactLong(obj: JSONObject, key: String): Long? = when (val value = obj.opt(key)) {
        is Int -> value.toLong()
        is Long -> value
        else -> null
    }

    private fun boundedCounter(obj: JSONObject, key: String): Boolean =
        exactLong(obj, key)?.let { it in 0L..MAX_PUBLIC_COUNTER } == true

    private fun nullableBoundedCounter(obj: JSONObject, key: String): Boolean =
        obj.isNull(key) || boundedCounter(obj, key)

    private fun nullableBoolean(obj: JSONObject, key: String): Boolean =
        obj.isNull(key) || obj.opt(key) is Boolean

    private fun nullableBoundedString(obj: JSONObject, key: String, maxBytes: Int): Boolean {
        if (obj.isNull(key)) return true
        val value = exactString(obj, key) ?: return false
        return boundedRequiredString(value, maxBytes)
    }

    private fun boundedRequiredString(value: String, maxBytes: Int): Boolean =
        value.isNotEmpty() &&
            value.toByteArray(Charsets.UTF_8).size <= maxBytes &&
            value.none { it == '\u0000' || it.code < 0x20 || it.code == 0x7f }

    private fun validateRequest(obj: JSONObject, type: String, id: String): BridgeValidation {
        if (!hasOnlyKeys(obj, setOf("t", "id", "method", "path", "headers", "body"))) {
            return BridgeValidation(false, type, id, "unknown_field")
        }
        val method = exactString(obj, "method")
        if (method !in setOf("GET", "POST")) {
            return BridgeValidation(false, type, id, "missing_method")
        }
        val path = exactString(obj, "path")
        if (
            path.isNullOrBlank() ||
            !path.startsWith('/') ||
            path.startsWith("//") ||
            path.contains('#') ||
            path.length > MAX_PATH_CHARS
        ) {
            return BridgeValidation(false, type, id, "missing_path")
        }
        if (obj.has("headers")) {
            val headers = obj.opt("headers") as? JSONObject
                ?: return BridgeValidation(false, type, id, "invalid_headers")
            val seen = HashSet<String>()
            val keys = headers.keys()
            while (keys.hasNext()) {
                val name = keys.next()
                val value = headers.opt(name)
                if (
                    !HEADER_NAME.matches(name) ||
                    value !is String ||
                    value.length > MAX_HEADER_VALUE_CHARS
                ) {
                    return BridgeValidation(false, type, id, "invalid_headers")
                }
                val normalized = name.lowercase()
                if (!seen.add(normalized) || normalized == "x-body-encoding") {
                    return BridgeValidation(false, type, id, "invalid_headers")
                }
            }
        }
        if (obj.has("body") && !obj.isNull("body") && obj.opt("body") !is String) {
            return BridgeValidation(false, type, id, "invalid_body")
        }
        return BridgeValidation(true, type, id)
    }

    private fun validateSubscription(obj: JSONObject, type: String, id: String): BridgeValidation {
        if (!hasOnlyKeys(obj, setOf("t", "id", "path"))) {
            return BridgeValidation(false, type, id, "unknown_field")
        }
        val path = exactString(obj, "path")
        return if (
            path.isNullOrBlank() ||
            !path.startsWith('/') ||
            path.startsWith("//") ||
            path.contains('#') ||
            path.length > MAX_PATH_CHARS
        ) {
            BridgeValidation(false, type, id, "missing_path")
        } else {
            BridgeValidation(true, type, id)
        }
    }

    private fun validateNative(obj: JSONObject, type: String, id: String): BridgeValidation {
        if (!hasOnlyKeys(obj, setOf("t", "id", "op", "payload"))) {
            return BridgeValidation(false, type, id, "unknown_field")
        }
        val op = exactString(obj, "op") ?: ""
        if (op.isBlank()) return BridgeValidation(false, type, id, "missing_op")
        val payload = obj.opt("payload") as? JSONObject
            ?: return BridgeValidation(false, type, id, "missing_payload")
        if (op == "openExternal") {
            if (
                !hasOnlyKeys(payload, setOf("url", "kind")) ||
                exactString(payload, "url").isNullOrBlank()
            ) {
                return BridgeValidation(false, type, id, "missing_url")
            }
            if (ExternalUrlPolicy.authKindFromWire(exactString(payload, "kind") ?: "") == null) {
                return BridgeValidation(false, type, id, "missing_or_unknown_kind")
            }
        }
        when (op) {
            "beginNetworkGuard" -> {
                if (
                    !hasOnlyKeys(payload, setOf("reason")) ||
                    NetworkGuardReason.fromWire(exactString(payload, "reason") ?: "") == null
                ) {
                    return BridgeValidation(false, type, id, "missing_or_unknown_guard_reason")
                }
            }
            "endNetworkGuard" -> {
                if (
                    !hasOnlyKeys(payload, setOf("guard_id")) ||
                    exactString(payload, "guard_id").isNullOrBlank()
                ) {
                    return BridgeValidation(false, type, id, "missing_guard_id")
                }
            }
            "bindNetworkGuard" -> {
                if (
                    !hasOnlyKeys(payload, setOf("guard_id", "turn")) ||
                    exactString(payload, "guard_id").isNullOrBlank()
                ) {
                    return BridgeValidation(false, type, id, "missing_guard_id")
                }
                if (!NetworkGuardPolicy.validTurnId(exactString(payload, "turn") ?: "")) {
                    return BridgeValidation(false, type, id, "invalid_turn")
                }
            }
            "captureNetworkSnapshot" -> {
                if (
                    !hasOnlyKeys(payload, setOf("guard_id")) ||
                    exactString(payload, "guard_id").isNullOrBlank()
                ) {
                    return BridgeValidation(false, type, id, "missing_guard_id")
                }
            }
            "openNetworkSettings" -> {
                if (
                    !hasOnlyKeys(payload, setOf("hint")) ||
                    NetworkSettingsHint.fromWire(exactString(payload, "hint") ?: "") == null
                ) {
                    return BridgeValidation(false, type, id, "missing_or_unknown_settings_hint")
                }
            }
            "pushToken" -> if (!hasOnlyKeys(payload, emptySet())) {
                return BridgeValidation(false, type, id, "unknown_field")
            }
            "openExternal" -> Unit
            else -> return BridgeValidation(false, type, id, "unknown_native_op")
        }
        return BridgeValidation(true, type, id)
    }

    private fun exactString(obj: JSONObject, key: String): String? = obj.opt(key) as? String

    private fun hasOnlyKeys(obj: JSONObject, allowed: Set<String>): Boolean {
        val keys = obj.keys()
        while (keys.hasNext()) {
            if (keys.next() !in allowed) return false
        }
        return true
    }

    private fun hasExactKeys(obj: JSONObject, expected: Set<String>): Boolean =
        obj.length() == expected.size && hasOnlyKeys(obj, expected)
}

private object StrictJsonSyntax {
    fun validateObject(input: String): Boolean = Scanner(input).validateObject()

    private class Scanner(private val input: String) {
        private var index = 0

        fun validateObject(): Boolean {
            skipWhitespace()
            if (!parseObject(0)) return false
            skipWhitespace()
            return index == input.length
        }

        private fun parseValue(depth: Int): Boolean {
            if (depth > 64 || index >= input.length) return false
            return when (input[index]) {
                '{' -> parseObject(depth)
                '[' -> parseArray(depth)
                '"' -> parseString() != null
                't' -> consumeLiteral("true")
                'f' -> consumeLiteral("false")
                'n' -> consumeLiteral("null")
                '-', in '0'..'9' -> parseNumber()
                else -> false
            }
        }

        private fun parseObject(depth: Int): Boolean {
            if (depth > 64 || !consume('{')) return false
            skipWhitespace()
            if (consume('}')) return true
            val keys = HashSet<String>()
            while (true) {
                skipWhitespace()
                val key = parseString() ?: return false
                if (!keys.add(key)) return false
                skipWhitespace()
                if (!consume(':')) return false
                skipWhitespace()
                if (!parseValue(depth + 1)) return false
                skipWhitespace()
                if (consume('}')) return true
                if (!consume(',')) return false
            }
        }

        private fun parseArray(depth: Int): Boolean {
            if (depth > 64 || !consume('[')) return false
            skipWhitespace()
            if (consume(']')) return true
            while (true) {
                if (!parseValue(depth + 1)) return false
                skipWhitespace()
                if (consume(']')) return true
                if (!consume(',')) return false
                skipWhitespace()
            }
        }

        private fun parseString(): String? {
            if (!consume('"')) return null
            val value = StringBuilder()
            while (index < input.length) {
                val current = input[index++]
                when {
                    current == '"' -> return value.toString()
                    current.code < 0x20 -> return null
                    current != '\\' -> {
                        when {
                            Character.isHighSurrogate(current) -> {
                                if (index >= input.length || !Character.isLowSurrogate(input[index])) {
                                    return null
                                }
                                value.append(current).append(input[index++])
                            }
                            Character.isLowSurrogate(current) -> return null
                            else -> value.append(current)
                        }
                    }
                    index >= input.length -> return null
                    else -> when (val escaped = input[index++]) {
                        '"', '\\', '/' -> value.append(escaped)
                        'b' -> value.append('\b')
                        'f' -> value.append('\u000c')
                        'n' -> value.append('\n')
                        'r' -> value.append('\r')
                        't' -> value.append('\t')
                        'u' -> {
                            if (index + 4 > input.length) return null
                            val code = input.substring(index, index + 4).toIntOrNull(16)
                                ?: return null
                            index += 4
                            val decoded = code.toChar()
                            when {
                                Character.isHighSurrogate(decoded) -> {
                                    if (
                                        index + 6 > input.length ||
                                        input[index] != '\\' ||
                                        input[index + 1] != 'u'
                                    ) {
                                        return null
                                    }
                                    val low = input.substring(index + 2, index + 6).toIntOrNull(16)
                                        ?.toChar() ?: return null
                                    if (!Character.isLowSurrogate(low)) return null
                                    value.append(decoded).append(low)
                                    index += 6
                                }
                                Character.isLowSurrogate(decoded) -> return null
                                else -> value.append(decoded)
                            }
                        }
                        else -> return null
                    }
                }
            }
            return null
        }

        private fun parseNumber(): Boolean {
            if (consume('-') && index >= input.length) return false
            if (consume('0')) {
                if (index < input.length && input[index] in '0'..'9') return false
            } else {
                if (index >= input.length || input[index] !in '1'..'9') return false
                while (index < input.length && input[index] in '0'..'9') index++
            }
            if (consume('.')) {
                if (index >= input.length || input[index] !in '0'..'9') return false
                while (index < input.length && input[index] in '0'..'9') index++
            }
            if (index < input.length && input[index] in "eE") {
                index++
                if (index < input.length && input[index] in "+-") index++
                if (index >= input.length || input[index] !in '0'..'9') return false
                while (index < input.length && input[index] in '0'..'9') index++
            }
            return true
        }

        private fun consumeLiteral(value: String): Boolean {
            if (!input.startsWith(value, index)) return false
            index += value.length
            return true
        }

        private fun consume(expected: Char): Boolean {
            if (index >= input.length || input[index] != expected) return false
            index++
            return true
        }

        private fun skipWhitespace() {
            while (index < input.length && input[index] in " \t\r\n") index++
        }
    }
}
