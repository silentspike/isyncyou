package com.silentspike.isyncyou

import androidx.test.platform.app.InstrumentationRegistry
import androidx.work.WorkInfo
import androidx.work.WorkManager
import org.junit.Test
import java.util.concurrent.TimeUnit

/** Explicitly selected debug-device probe; no account contents or tokens are printed. */
class ProviderRefreshInstrumentedTest {
    @Test fun accountCatchUpIsScheduled() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        check(EngineBootstrap.ensureStarted(context.filesDir).isNotEmpty()) { "engine unavailable" }
        val jobs = WorkManager.getInstance(context).getWorkInfosForUniqueWork("provider-account-catch-up")
            .get(10, TimeUnit.SECONDS)
        val active = jobs.filter { !it.state.isFinished }
        check(active.size == 1 && active.single().state in setOf(WorkInfo.State.ENQUEUED, WorkInfo.State.RUNNING)) {
            "account catch-up unavailable"
        }
    }

    @Test fun protectedProviderCatchUpCompletes() {
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        val account = requireNotNull(InstrumentationRegistry.getArguments().getString("testAccount"))
        check(account.matches(Regex("[A-Za-z0-9._-]{1,128}"))) { "invalid account" }
        val context = instrumentation.targetContext
        check(EngineBootstrap.ensureStarted(context.filesDir).isNotEmpty()) { "engine unavailable" }
        check(NativeEngine.nativeWaitProviderRefresh(account) == 1) { "provider pass incomplete" }
    }
}
