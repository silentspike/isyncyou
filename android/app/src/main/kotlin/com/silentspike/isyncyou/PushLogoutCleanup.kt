package com.silentspike.isyncyou

import android.content.Context
import org.json.JSONObject
import java.security.MessageDigest

/** The distributor request has no acknowledgement; an interrupted call stays unknown. */
internal object PushLogoutCleanup {
    private fun slot(account: String): String {
        require(account.matches(Regex("[A-Za-z0-9._-]{1,128}")))
        val hash = MessageDigest.getInstance("SHA-256").digest(account.toByteArray(Charsets.UTF_8))
        return "c_" + hash.joinToString("") { "%02x".format(it.toInt() and 255) }
    }

    private fun logoutSlot(account: String) = "l_" + slot(account).removePrefix("c_")

    fun getLogout(context: Context, account: String): JSONObject? = PushEnrollmentStore.get(context, logoutSlot(account))
    fun putLogout(context: Context, account: String, state: JSONObject) = PushEnrollmentStore.put(context, logoutSlot(account), state)
    fun finishLogout(context: Context, account: String) = PushEnrollmentStore.remove(context, logoutSlot(account))

    fun resumeLogout(state: JSONObject, save: (JSONObject) -> Unit, cleanup: () -> Unit,
                     providerLogout: () -> Unit, finish: () -> Unit) {
        if (!state.optBoolean("push_done")) {
            cleanup()
            state.put("push_done", true)
            save(state)
        }
        providerLogout()
        finish()
    }

    fun get(context: Context, account: String): JSONObject? = PushEnrollmentStore.get(context, slot(account))
    fun put(context: Context, account: String, state: JSONObject) = PushEnrollmentStore.put(context, slot(account), state)
    fun remove(context: Context, account: String) = PushEnrollmentStore.remove(context, slot(account))

    fun archive(context: Context, account: String, state: JSONObject) {
        check(state.getString("account") == account && state.getString("distributor") == "submitted" && state.getBoolean("local_done"))
        val instance = state.getString("installation")
        val hash = MessageDigest.getInstance("SHA-256").digest(instance.toByteArray(Charsets.UTF_8))
        val key = "p_" + hash.joinToString("") { "%02x".format(it.toInt() and 255) }
        PushEnrollmentStore.put(context, key, state)
        remove(context, account)
    }

    /** Persist the unknown boundary before entering the connector, never replay it on recovery. */
    fun resume(state: JSONObject, save: (JSONObject) -> Unit, unregister: () -> Unit,
               localCleanup: () -> Unit, remove: () -> Unit) {
        when (state.getString("distributor")) {
            "ready" -> {
                state.put("distributor", "unknown")
                save(state)
                if (runCatching { unregister() }.isSuccess) {
                    state.put("distributor", "submitted")
                    save(state)
                }
            }
            "unknown", "submitted" -> Unit
            else -> error("Invalid cleanup state")
        }
        if (!state.optBoolean("local_done")) {
            localCleanup()
            state.put("local_done", true)
            save(state)
        }
        if (state.getString("distributor") == "submitted") remove()
    }
}
