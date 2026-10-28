package com.silentspike.isyncyou

import android.content.Context
import org.json.JSONObject
import androidx.work.ExistingWorkPolicy
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.Constraints
import androidx.work.NetworkType
import androidx.work.workDataOf
import java.text.SimpleDateFormat
import java.util.Locale
import java.util.TimeZone
import java.util.concurrent.TimeUnit

/** Native-only JMAP operations. Responses can contain endpoint capabilities. */
internal object PushJmap {
    private fun format(pattern: String) = SimpleDateFormat(pattern, Locale.US).apply {
        timeZone = TimeZone.getTimeZone("UTC"); isLenient = false
    }

    fun expiryMillis(value: String): Long? = listOf("yyyy-MM-dd'T'HH:mm:ss.SSS'Z'", "yyyy-MM-dd'T'HH:mm:ss'Z'")
        .firstNotNullOfOrNull { pattern -> runCatching { format(pattern).parse(value)?.time }.getOrNull() }

    fun requestedExpiry(): String = format("yyyy-MM-dd'T'HH:mm:ss'Z'")
        .format(java.util.Date(System.currentTimeMillis() + TimeUnit.DAYS.toMillis(6)))

    fun scheduleRenewal(context: Context, instance: String, expires: String) {
        val deadline = requireNotNull(expiryMillis(expires))
        val delay = (deadline - TimeUnit.DAYS.toMillis(1) - System.currentTimeMillis()).coerceAtLeast(60_000)
        val request = OneTimeWorkRequestBuilder<PushEnrollmentWorker>()
            .setInputData(workDataOf("instance" to instance))
            .setConstraints(Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build())
            .setInitialDelay(delay, TimeUnit.MILLISECONDS).build()
        WorkManager.getInstance(context).enqueueUniqueWork("provider-push-renew:$instance:$expires", ExistingWorkPolicy.KEEP, request)
    }
    fun call(context: Context, account: String, operation: String, body: JSONObject = JSONObject()): JSONObject {
        val response = JSONObject(NativeEngine.nativeProviderPush(context.filesDir.absolutePath, account, operation, body.toString()))
        check(response.optBoolean("ok")) { "Push operation unavailable" }
        return response.getJSONObject("data")
    }

    fun find(context: Context, account: String, instance: String, endpoint: String, publicKey: String, auth: String): JSONObject? {
        val response = call(context, account, "get")
        val methods = response.getJSONArray("methodResponses")
        if (methods.length() != 1) return null
        val method = methods.getJSONArray(0)
        if (method.getString(0) != "PushSubscription/get") return null
        val items = method.getJSONObject(1).getJSONArray("list")
        var result: JSONObject? = null
        for (i in 0 until items.length()) {
            val item = items.getJSONObject(i)
            val keys = item.optJSONObject("keys")
            if (item.optString("deviceClientId") == instance && item.optString("url") == endpoint &&
                keys != null && keys.optString("p256dh") == publicKey && keys.optString("auth") == auth) {
                if (result != null) return null
                result = item
            }
        }
        return result
    }
}
