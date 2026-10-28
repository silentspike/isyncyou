package com.silentspike.isyncyou

import android.content.Context
import android.util.Base64
import org.json.JSONObject

/** OAuth bearer transport is native; signed broker challenges stay outside the WebView. */
internal object PushBroker {
    fun operation(context: Context, account: String, endpoint: String, action: String, nonce: String? = null): JSONObject {
        require(action in setOf("prepare", "deliver", "confirm", "revoke"))
        val identity = PushInstallationKey.getOrCreate(context, account)
        val request = JSONObject().put("action", action).put("installation_id", identity.installation)
            .put("endpoint", endpoint).put("public_key", identity.publicKey)
        val challenge = call(context, account, "challenge", request)
        val id = challenge.getString("challenge_id")
        val bytes = Base64.decode(challenge.getString("payload"), Base64.URL_SAFE or Base64.NO_PADDING or Base64.NO_WRAP)
        require(bytes.size in 1..8192)
        val payload = JSONObject(String(bytes, Charsets.UTF_8))
        require(payload.getInt("version") == 1 && payload.getString("challenge_id") == id &&
            payload.getString("action") == action && payload.getString("installation_id") == identity.installation &&
            payload.getString("endpoint") == endpoint && payload.getString("public_key") == identity.publicKey &&
            payload.getLong("expires") >= System.currentTimeMillis() / 1000)
        val signature = PushInstallationKey.sign(context, account, bytes)
        bytes.fill(0)
        val body = JSONObject().put("challenge_id", id).put("signature", signature)
        if (nonce != null) body.put("received_nonce", nonce)
        return call(context, account, action, body)
    }

    private fun call(context: Context, account: String, operation: String, body: JSONObject): JSONObject {
        val response = JSONObject(NativeEngine.nativeProviderBroker(context.filesDir.absolutePath, account, operation, body.toString()))
        check(response.optBoolean("ok", false)) { "Push broker operation unavailable" }
        return response.getJSONObject("data")
    }
}
