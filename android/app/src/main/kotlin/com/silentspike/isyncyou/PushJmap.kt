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
    enum class Lookup { Present, Absent }
    private fun format(pattern: String) = SimpleDateFormat(pattern, Locale.US).apply {
        timeZone = TimeZone.getTimeZone("UTC"); isLenient = false
    }

    fun expiryMillis(value: String): Long? = listOf("yyyy-MM-dd'T'HH:mm:ss.SSS'Z'", "yyyy-MM-dd'T'HH:mm:ss'Z'")
        .firstNotNullOfOrNull { pattern ->
            runCatching {
                val position = java.text.ParsePosition(0)
                val parsed = format(pattern).parse(value, position)
                if (position.index == value.length) parsed?.time else null
            }.getOrNull()
        }

    fun requestedExpiry(): String = format("yyyy-MM-dd'T'HH:mm:ss'Z'")
        .format(java.util.Date(System.currentTimeMillis() + TimeUnit.DAYS.toMillis(6)))

    fun scheduleRenewal(context: Context, instance: String, expires: String) {
        val deadline = requireNotNull(expiryMillis(expires))
        val delay = (deadline - TimeUnit.DAYS.toMillis(1) - System.currentTimeMillis()).coerceAtLeast(60_000)
        val request = OneTimeWorkRequestBuilder<PushEnrollmentWorker>()
            .setInputData(workDataOf("instance" to instance))
            .setConstraints(Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build())
            .addTag(renewalTag(instance))
            .setInitialDelay(delay, TimeUnit.MILLISECONDS).build()
        WorkManager.getInstance(context).enqueueUniqueWork("provider-push-renew:$instance:$expires", ExistingWorkPolicy.KEEP, request)
    }
    fun renewalTag(instance: String) = "provider-push-renew:$instance"
    fun call(context: Context, account: String, operation: String, body: JSONObject = JSONObject()): JSONObject {
        val response = JSONObject(NativeEngine.nativeProviderPush(context.filesDir.absolutePath, account, operation, body.toString()))
        check(response.optBoolean("ok")) { "Push operation unavailable" }
        return response.getJSONObject("data")
    }

    /** Native servers need not expose the endpoint or encryption keys on get. */
    fun findKnown(context: Context, account: String, instance: String, id: String): JSONObject? {
        val response = call(context, account, "get")
        val methods = response.getJSONArray("methodResponses")
        if (methods.length() != 1) return null
        val method = methods.getJSONArray(0)
        if (method.getString(0) != "PushSubscription/get") return null
        val items = method.getJSONObject(1).getJSONArray("list")
        var result: JSONObject? = null
        for (i in 0 until items.length()) {
            val item = items.getJSONObject(i)
            if (item.optString("deviceClientId") == instance) {
                if (item.optString("id") != id) return null
                if (result != null) return null
                result = item
            }
        }
        return result
    }

    /** Absence is accepted only from an explicit, single-ID authenticated notFound. */
    fun lookup(context: Context, account: String, instance: String, id: String): Lookup {
        val response = call(context, account, "get", JSONObject().put("id", id))
        val methods = response.getJSONArray("methodResponses")
        require(methods.length() == 1)
        val method = methods.getJSONArray(0)
        require(method.getString(0) == "PushSubscription/get")
        val result = method.getJSONObject(1)
        val list = result.getJSONArray("list")
        val missing = result.getJSONArray("notFound")
        if (list.length() == 0 && missing.length() == 1 && missing.getString(0) == id) return Lookup.Absent
        require(list.length() == 1 && missing.length() == 0)
        val item = list.getJSONObject(0)
        require(item.getString("id") == id && item.getString("deviceClientId") == instance)
        return Lookup.Present
    }
}
