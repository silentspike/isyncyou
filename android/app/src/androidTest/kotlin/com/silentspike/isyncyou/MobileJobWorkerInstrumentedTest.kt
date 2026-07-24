package com.silentspike.isyncyou

import android.os.Process
import androidx.test.platform.app.InstrumentationRegistry
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.WorkInfo
import androidx.work.WorkManager
import java.util.concurrent.TimeUnit
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class MobileJobWorkerInstrumentedTest {
    @Test
    fun activityAndWorkManagerUseSameProcess() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val manager = WorkManager.getInstance(context)
        val request = OneTimeWorkRequestBuilder<MobileJobProcessProbeWorker>().build()
        manager.enqueue(request).result.get(15, TimeUnit.SECONDS)

        val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(30)
        var info: WorkInfo
        do {
            info = manager.getWorkInfoById(request.id).get(5, TimeUnit.SECONDS)
            if (info.state.isFinished) break
            Thread.sleep(100)
        } while (System.nanoTime() < deadline)

        assertTrue("process probe worker did not finish", info.state.isFinished)
        assertEquals(WorkInfo.State.SUCCEEDED, info.state)
        assertEquals(Process.myPid(), info.outputData.getInt("pid", -1))
        assertEquals(currentProcessName(context), info.outputData.getString("process_name"))
    }
}
