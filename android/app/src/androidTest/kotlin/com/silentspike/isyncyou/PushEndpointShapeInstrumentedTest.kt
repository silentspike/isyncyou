package com.silentspike.isyncyou

import androidx.test.platform.app.InstrumentationRegistry
import android.system.Os
import android.system.OsConstants
import org.json.JSONObject
import org.junit.Test
import java.io.File
import java.io.FileOutputStream
import java.net.URI

/** Read-only metadata: never emit an endpoint, topic, key or credential. */
class PushEndpointShapeInstrumentedTest {
    /** One-shot app-private transfer; contents must be consumed by the private harness. */
    @Test fun exportBoundIdentityOnce() {
        val account = requireNotNull(InstrumentationRegistry.getArguments().getString("testAccount"))
        check(account.matches(Regex("[A-Za-z0-9._-]{1,128}"))) { "invalid scope" }
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        check(BuildConfig.DEBUG) { "debug build required" }
        check(EngineBootstrap.ensureStarted(context.filesDir).isNotEmpty()) { "engine unavailable" }
        val identity = requireNotNull(PushInstallationKey.existing(context, account))
        val state = requireNotNull(PushEnrollmentStore.get(context, identity.installation))
        check(state.optString("stage") == "prepare_unknown" && state.optString("account") == account &&
            state.optString("origin") == NativeEngine.nativeProviderOrigin(context.filesDir.absolutePath, account)) { "binding changed" }
        val body = JSONObject().put("installation_id", identity.installation)
            .put("endpoint", state.getString("endpoint")).put("public_key", identity.publicKey).toString().toByteArray()
        val file = File(context.filesDir, "wp16-push-binding-once.json")
        val fd = Os.open(file.absolutePath, OsConstants.O_WRONLY or OsConstants.O_CREAT or OsConstants.O_EXCL or OsConstants.O_NOFOLLOW, 0x180)
        try {
            FileOutputStream(fd).use {
                val stat = Os.fstat(fd)
                check(OsConstants.S_ISREG(stat.st_mode) && stat.st_uid == android.os.Process.myUid() &&
                    stat.st_nlink == 1L && stat.st_mode and 0x1ff == 0x180) { "private transfer unavailable" }
                it.write(body); it.fd.sync()
            }
        } finally { body.fill(0) }
    }

    @Test fun inspectEndpointShape() {
        val args = InstrumentationRegistry.getArguments()
        val account = requireNotNull(args.getString("testAccount"))
        val expectedHost = requireNotNull(args.getString("expectedHost"))
        check(account.matches(Regex("[A-Za-z0-9._-]{1,128}")) &&
            expectedHost.matches(Regex("[A-Za-z0-9.-]{1,253}"))) { "invalid diagnostic scope" }
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val identity = requireNotNull(PushInstallationKey.existing(context, account))
        val state = requireNotNull(PushEnrollmentStore.get(context, identity.installation))
        val endpoint = URI(state.getString("endpoint"))
        val shape = JSONObject()
            .put("https", endpoint.scheme == "https")
            .put("host_matches", endpoint.host == expectedHost && (endpoint.port == -1 || endpoint.port == 443))
            .put("no_userinfo_fragment", endpoint.rawUserInfo == null && endpoint.rawFragment == null)
            .put("query_matches", endpoint.rawQuery == "up=1")
            .put("topic_shape", endpoint.rawPath?.matches(Regex("/up[A-Za-z0-9]{12}")) == true)
            .put("prepare_unknown", state.optString("stage") == "prepare_unknown")
        println("WP16_PUSH_ENDPOINT_SHAPE=$shape")
    }
}
