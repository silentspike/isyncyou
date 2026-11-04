package com.silentspike.isyncyou

/** Unknown and fenced outcomes must remain visible even if delivery degrades. */
internal object PushStatusPolicy {
    fun retainStage(stage: String): Boolean = stage.endsWith("_unknown") ||
        stage == "prepare_cancelled" || stage == "prepare_cancel_conflict" || stage == "distributor_detached"
}
