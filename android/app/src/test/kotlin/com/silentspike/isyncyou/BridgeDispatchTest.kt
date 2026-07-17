package com.silentspike.isyncyou

import java.util.concurrent.Executor
import java.util.concurrent.RejectedExecutionException
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class BridgeDispatchTest {
    @Test
    fun acceptedDispatchRunsTask() {
        var ran = false

        assertTrue(BridgeDispatch.execute(Executor { it.run() }) { ran = true })
        assertTrue(ran)
    }

    @Test
    fun dispatchAfterExecutorShutdownIsIgnored() {
        var ran = false
        val rejected = Executor { throw RejectedExecutionException("shutdown") }

        assertFalse(BridgeDispatch.execute(rejected) { ran = true })
        assertFalse(ran)
    }
}
