package com.silentspike.isyncyou

import androidx.test.platform.app.InstrumentationRegistry
import org.json.JSONObject
import org.junit.Test

/** Reads only the explicitly selected debug account; no subscription mutation. */
class PushMetadataReadOnlyInstrumentedTest {
    @Test fun inspectBrokerBoundMetadata() {
        check(BuildConfig.DEBUG)
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        val account = requireNotNull(InstrumentationRegistry.getArguments().getString("testAccount"))
        check(account.matches(Regex("[A-Za-z0-9._-]{1,128}")))
        val context = instrumentation.targetContext
        check(EngineBootstrap.ensureStarted(context.filesDir).isNotEmpty())
        val identity = requireNotNull(PushInstallationKey.existing(context, account))
        val state = requireNotNull(PushEnrollmentStore.get(context, identity.installation))
        check(state.optString("stage") == "broker_bound" && state.optString("account") == account &&
            state.optString("origin") == NativeEngine.nativeProviderOrigin(context.filesDir.absolutePath, account))
        val result = try {
            val metadata = PushJmap.call(context, account, "metadata")
            val oldVapid = if (state.isNull("vapid")) null else state.optString("vapid")
            val newVapid = if (metadata.isNull("vapid")) null else metadata.optString("vapid")
            JSONObject().put("metadata", "ok")
                .put("vapid_matches", oldVapid == newVapid)
                .put("session_matches", state.optString("session_state") == metadata.optString("session_state"))
        } catch (_: Exception) {
            JSONObject().put("metadata", "unavailable")
        }
        println("WP16_PUSH_METADATA_READ_ONLY=$result")
    }
}
