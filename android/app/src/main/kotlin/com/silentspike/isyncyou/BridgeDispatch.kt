package com.silentspike.isyncyou

import java.util.concurrent.Executor
import java.util.concurrent.RejectedExecutionException

internal object BridgeDispatch {
    fun execute(executor: Executor, block: () -> Unit): Boolean =
        try {
            executor.execute(block)
            true
        } catch (_: RejectedExecutionException) {
            false
        }
}
