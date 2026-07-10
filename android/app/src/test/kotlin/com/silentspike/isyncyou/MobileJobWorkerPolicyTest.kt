package com.silentspike.isyncyou

import androidx.work.Data
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class MobileJobWorkerPolicyTest {
    @Test
    fun accepts_only_bounded_job_id_and_known_kind() {
        val data = Data.Builder().putString("job_id", "job-1").putString("kind", "backup").build()
        assertEquals(MobileJobKindWire.BACKUP, MobileJobWorkerPolicy.parseInput(data)?.kind)
        val bad = Data.Builder().putString("job_id", "../secret").putString("kind", "backup").build()
        assertNull(MobileJobWorkerPolicy.parseInput(bad))
    }

    @Test
    fun rejects_unknown_or_missing_work_input() {
        assertNull(MobileJobWorkerPolicy.parseInput(Data.Builder().putString("job_id", "x").build()))
        assertNull(MobileJobWorkerPolicy.parseInput(
            Data.Builder().putString("job_id", "x").putString("kind", "live-write").build(),
        ))
        assertNull(MobileJobWorkerPolicy.parseInput(
            Data.Builder().putString("job_id", "x").putString("kind", "backup")
                .putString("account", "must-not-be-scheduled").build(),
        ))
    }

    @Test
    fun parses_bounded_native_outcomes_only() {
        assertEquals(
            MobileJobWorkerPolicy.WorkerResult.Retry,
            MobileJobWorkerPolicy.resultFor(
                MobileJobWorkerPolicy.parseResponse("{\"v\":1,\"status\":\"retry\",\"code\":\"network\"}")!!,
            ),
        )
        assertNull(MobileJobWorkerPolicy.parseResponse("{\"v\":2,\"status\":\"succeeded\"}"))
    }

    @Test
    fun worker_policy_never_starts_native_before_foreground_and_snapshot() {
        val events = mutableListOf<String>()
        val controller = MobileJobWorkerController(
            foreground = object : MobileJobForegroundController {
                override suspend fun publish(job: MobileJobInput): Boolean {
                    events += "foreground"
                    return true
                }
            },
            deviceState = object : MobileDeviceStateProvider {
                override fun snapshot(): MobileDeviceSnapshot? {
                    events += "snapshot"
                    return MobileDeviceSnapshot(true, false, true, 1000)
                }
            },
            native = object : MobileJobNativeController {
                override fun start(filesDir: java.io.File): Boolean {
                    events += "start"
                    return true
                }
                override fun run(input: MobileJobInput, device: MobileDeviceSnapshot): String {
                    events += "run"
                    return "{\"v\":1,\"status\":\"succeeded\"}"
                }
            },
            filesDir = java.io.File("/tmp/626-test"),
        )
        kotlinx.coroutines.runBlocking {
            assertEquals(
                MobileJobWorkerPolicy.WorkerResult.Success,
                controller.run(MobileJobInput("job-1", MobileJobKindWire.BACKUP)).result,
            )
        }
        assertEquals(listOf("foreground", "snapshot", "start", "run"), events)
        assertTrue(events.indexOf("foreground") < events.indexOf("start"))
    }

    @Test
    fun notification_failure_never_reaches_device_or_native() {
        val events = mutableListOf<String>()
        val controller = MobileJobWorkerController(
            foreground = object : MobileJobForegroundController {
                override suspend fun publish(job: MobileJobInput): Boolean = false
            },
            deviceState = object : MobileDeviceStateProvider {
                override fun snapshot(): MobileDeviceSnapshot? {
                    events += "snapshot"
                    return MobileDeviceSnapshot(true, false, true, 1000)
                }
            },
            native = object : MobileJobNativeController {
                override fun start(filesDir: java.io.File): Boolean {
                    events += "start"
                    return true
                }
                override fun run(input: MobileJobInput, device: MobileDeviceSnapshot): String {
                    events += "run"
                    return "{\"v\":1,\"status\":\"succeeded\"}"
                }
            },
            filesDir = java.io.File("/tmp/626-test"),
        )
        val outcome = kotlinx.coroutines.runBlocking {
            controller.run(MobileJobInput("job-1", MobileJobKindWire.BACKUP))
        }
        assertEquals(MobileJobWorkerPolicy.WorkerResult.Failure, outcome.result)
        assertEquals("notifications_required", outcome.code)
        assertTrue(events.isEmpty())
    }
}
