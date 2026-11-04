package com.silentspike.isyncyou

import android.content.Context
import android.util.Base64
import org.json.JSONObject

internal fun pushAccountBinding(nativeAccount: String): String {
    val digest = java.security.MessageDigest.getInstance("SHA-256")
    digest.update("isyncyou-push-account-v1".toByteArray(Charsets.UTF_8))
    digest.update(0.toByte())
    return digest.digest(nativeAccount.toByteArray(Charsets.UTF_8))
        .joinToString("") { "%02x".format(it.toInt() and 0xff) }
}

/** OAuth bearer transport is native; signed broker challenges stay outside the WebView. */
internal object PushBroker {
    fun operation(context: Context, account: String, endpoint: String, action: String, nonce: String? = null,
                  beforeDispatch: (() -> Boolean)? = null, operationId: String? = null,
                  expectedAccountBinding: String? = null): JSONObject {
        require(action in setOf("prepare", "deliver", "confirm", "revoke", "cancel", "status"))
        val identity = if (action in setOf("cancel", "status")) requireNotNull(PushInstallationKey.existing(context, account))
            else PushInstallationKey.getOrCreate(context, account)
        val request = JSONObject().put("action", action).put("installation_id", identity.installation)
            .put("endpoint", endpoint).put("public_key", identity.publicKey)
        if (operationId != null) {
            require(action in setOf("status", "revoke") && operationId.matches(Regex("[0-9a-f]{32}")))
            request.put("operation_id", operationId)
        }
        return PushDispatchGate.run(challenge = {
            val challenge = call(context, account, "challenge", request)
            val id = challenge.getString("challenge_id")
            val bytes = Base64.decode(challenge.getString("payload"), Base64.URL_SAFE or Base64.NO_PADDING or Base64.NO_WRAP)
            try {
                require(bytes.size in 1..8192)
                val payload = JSONObject(String(bytes, Charsets.UTF_8))
                val now = System.currentTimeMillis() / 1000
                require(payload.getInt("version") == 1 && payload.getString("challenge_id") == id &&
                    payload.getString("action") == action && payload.getString("installation_id") == identity.installation &&
                    payload.getString("endpoint") == endpoint && payload.getString("public_key") == identity.publicKey &&
                    (operationId == null || payload.optString("operation_id") == operationId) &&
                    payload.getLong("expires") in now..(now + 180))
                if (expectedAccountBinding != null) {
                    require(action == "status" && expectedAccountBinding.matches(Regex("[0-9a-f]{64}")))
                    val actual = pushAccountBinding(payload.getString("account"))
                    require(java.security.MessageDigest.isEqual(actual.toByteArray(Charsets.US_ASCII), expectedAccountBinding.toByteArray(Charsets.US_ASCII)))
                }
                val signature = PushInstallationKey.sign(context, account, bytes)
                JSONObject().put("challenge_id", id).put("signature", signature).apply {
                    if (nonce != null) put("received_nonce", nonce)
                }
            } finally { bytes.fill(0) }
        }, claim = { beforeDispatch?.invoke() != false }, dispatch = { body ->
            call(context, account, action, body)
        })
    }

    private fun call(context: Context, account: String, operation: String, body: JSONObject): JSONObject {
        val response = JSONObject(NativeEngine.nativeProviderBroker(context.filesDir.absolutePath, account, operation, body.toString()))
        check(response.optBoolean("ok", false)) { "Push broker operation unavailable" }
        return response.getJSONObject("data")
    }
}
