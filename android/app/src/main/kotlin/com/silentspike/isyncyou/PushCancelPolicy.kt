package com.silentspike.isyncyou

/** A broker fence does not remove an existing binding. */
internal object PushCancelPolicy {
    fun canClaim(stage: String): Boolean = stage == "prepare_unknown" || stage == "prepare_cancel_unknown"
    fun canDetach(stage: String): Boolean = stage == "prepare_cancelled"
    fun canRegisterReplacement(stage: String): Boolean = stage == "distributor_detached"
    fun confirmedStage(binding: String): String? = when (binding) {
        "absent" -> "prepare_cancelled"
        "present", "conflict" -> "prepare_cancel_conflict"
        else -> null
    }
}
