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

    @Test fun selectedAccountsDiagnosticPass() {
        check(BuildConfig.DEBUG) { "debug build required" }
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        val args = InstrumentationRegistry.getArguments()
        val primary = requireNotNull(args.getString("primaryAccount"))
        val secondary = requireNotNull(args.getString("secondaryAccount"))
        check(primary != secondary && listOf(primary, secondary).all { it.matches(Regex("[A-Za-z0-9._-]{1,128}")) }) { "invalid account scope" }
        check(EngineBootstrap.ensureStarted(instrumentation.targetContext.filesDir).isNotEmpty()) { "engine unavailable" }
        val success = NativeEngine.nativeWaitProviderRefresh(secondary) == 1
        val operations = setOf("none", "store", "mailbox_query", "mailbox_get", "mailbox_changes", "email_query", "email_get", "email_changes")
        val classes = setOf("unavailable", "store", "http", "transport", "invalid_response", "unsupported", "outcome_unknown", "method")
        val methods = setOf("cannotCalculateChanges", "stateMismatch", "accountNotFound", "accountNotSupportedByMethod", "accountReadOnly", "forbidden", "invalidArguments", "serverFail", "unknownMethod", "requestTooLarge", "other")
        for ((role, account) in listOf("primary_fixture" to primary, "secondary_fixture" to secondary)) {
            val d = org.json.JSONObject(NativeEngine.nativeProviderRefreshDiagnostic(account))
            val op = d.optString("operation"); val kind = d.optString("class")
            val http = if (d.isNull("http_status")) 0 else d.optInt("http_status", -1)
            val method = if (d.isNull("method_type")) "none" else d.optString("method_type")
            check(op in operations && kind in classes && (http == 0 || http in 100..599) && (method == "none" || method in methods)) { "diagnostic unavailable" }
            check(d.has("pending") && d.opt("pending") is Boolean) { "diagnostic unavailable" }
            val pending = d.getBoolean("pending")
            println("WP16_REFRESH_DIAGNOSTIC role=$role pending=$pending operation=$op class=$kind http=$http method=$method")
        }
        println("WP16_REFRESH_SECONDARY_SUCCESS=$success")
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
