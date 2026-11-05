package com.silentspike.isyncyou

/** Persist the operation claim only after a challenge exists and before dispatch. */
internal object PushDispatchGate {
    fun <T, R> run(challenge: () -> T, claim: () -> Boolean, dispatch: (T) -> R): R {
        val prepared = challenge()
        check(claim()) { "Push operation state changed" }
        return dispatch(prepared)
    }
}
