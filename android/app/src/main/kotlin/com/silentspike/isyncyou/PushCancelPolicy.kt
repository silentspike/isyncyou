package com.silentspike.isyncyou

/** A broker fence does not remove an existing binding. */
internal object PushCancelPolicy {
    fun canClaim(stage: String, expires: Long = 0, now: Long = System.currentTimeMillis() / 1000): Boolean =
        stage == "prepare_unknown" || stage == "prepare_cancel_unknown" ||
            (stage == "credentials_ready" && expires > 0 && expires <= now)
    fun canDetach(stage: String): Boolean = stage == "prepare_cancelled"
    fun canRegisterReplacement(stage: String): Boolean = stage == "distributor_detached"
    fun confirmedStage(binding: String): String? = when (binding) {
        "absent" -> "prepare_cancelled"
        "present", "conflict" -> "prepare_cancel_conflict"
        else -> null
    }
}
