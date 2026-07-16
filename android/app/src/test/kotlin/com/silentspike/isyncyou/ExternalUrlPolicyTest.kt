package com.silentspike.isyncyou

import java.net.URLEncoder
import java.nio.charset.StandardCharsets
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ExternalUrlPolicyTest {
    private val accountScopes = listOf(
        "Files.ReadWrite",
        "Mail.ReadWrite",
        "Mail.Send",
        "MailboxSettings.ReadWrite",
        "Calendars.ReadWrite",
        "Contacts.ReadWrite",
        "Tasks.ReadWrite",
        "Notes.ReadWrite",
        "offline_access",
    )

    @Test
    fun externalUrlRequiresKnownKind() {
        val missing = ExternalUrlPolicy.classifyExternalUrl(
            "https://auth.openai.com/oauth/authorize",
            null,
        )
        assertFalse(missing.allowed)
        assertEquals("missing_or_unknown_kind", missing.reason)

        val unknown = ExternalUrlPolicy.classifyExternalUrl(
            "https://auth.openai.com/oauth/authorize",
            "account",
        )
        assertFalse(unknown.allowed)
        assertEquals("missing_or_unknown_kind", unknown.reason)
    }

    @Test
    fun agentAuthorizeUrlsRequireExactAllowlist() {
        assertAllowed(
            ExternalUrlPolicy.classifyExternalUrl(
                "https://claude.com/cai/oauth/authorize?client_id=abc",
                AuthUrlKind.AgentAuthorize,
            ),
            "claude.com",
        )
        assertAllowed(
            ExternalUrlPolicy.classifyExternalUrl(
                "https://auth.openai.com/oauth/authorize",
                AuthUrlKind.AgentAuthorize,
            ),
            "auth.openai.com",
        )
        assertAllowed(
            ExternalUrlPolicy.classifyExternalUrl(
                "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
                AuthUrlKind.AgentAuthorize,
            ),
            "login.microsoftonline.com",
        )
        assertBlocked(
            ExternalUrlPolicy.classifyExternalUrl(
                "https://claude.com/cai/oauth/callback",
                AuthUrlKind.AgentAuthorize,
            ),
            "not_allowlisted",
        )
        assertBlocked(
            ExternalUrlPolicy.classifyExternalUrl(
                "https://evil-login.microsoftonline.com.example/common/oauth2/v2.0/authorize",
                AuthUrlKind.AgentAuthorize,
            ),
            "not_allowlisted",
        )
    }

    @Test
    fun accountDeviceCodeUrlsAreASeparateBackendApprovedClass() {
        assertAllowed(
            ExternalUrlPolicy.classifyExternalUrl(
                "https://microsoft.com/devicelogin",
                AuthUrlKind.AccountDeviceCode,
            ),
            "microsoft.com",
        )
        assertAllowed(
            ExternalUrlPolicy.classifyExternalUrl(
                "https://www.microsoft.com/link",
                AuthUrlKind.AccountDeviceCode,
            ),
            "www.microsoft.com",
        )
        assertAllowed(
            ExternalUrlPolicy.classifyExternalUrl(
                "https://login.live.com/oauth20_remoteconnect.srf",
                AuthUrlKind.AccountDeviceCode,
            ),
            "login.live.com",
        )
        assertBlocked(
            ExternalUrlPolicy.classifyExternalUrl(
                "https://www.microsoft.com/oauth20_authorize.srf",
                AuthUrlKind.AccountDeviceCode,
            ),
            "not_allowlisted",
        )
    }

    @Test
    fun accountAuthorizeRequiresExactPkceAccountPickerContract() {
        assertAllowed(
            ExternalUrlPolicy.classifyExternalUrl(
                accountAuthorizeUrl(),
                AuthUrlKind.AccountAuthorize,
            ),
            "login.microsoftonline.com",
        )

        val invalidUrls = listOf(
            accountAuthorizeUrl(mapOf("prompt" to "login")),
            accountAuthorizeUrl(mapOf("client_id" to "other-client")),
            accountAuthorizeUrl(mapOf("redirect_uri" to "http://127.0.0.1:45678")),
            accountAuthorizeUrl(mapOf("redirect_uri" to "http://localhost:45678/")),
            accountAuthorizeUrl(mapOf("redirect_uri" to "http://localhost:45678/oauth/microsoft/callback")),
            accountAuthorizeUrl(mapOf("scope" to "Files.Read offline_access")),
            accountAuthorizeUrl(mapOf("state" to "short")),
            accountAuthorizeUrl().replace("login.microsoftonline.com/", "login.microsoftonline.com:444/"),
            accountAuthorizeUrl() + "&state=${"b".repeat(43)}",
        )
        for (url in invalidUrls) {
            assertBlocked(
                ExternalUrlPolicy.classifyExternalUrl(url, AuthUrlKind.AccountAuthorize),
                "not_allowlisted",
            )
        }
    }

    @Test
    fun unsafeUrlShapesAreRejectedBeforeAllowlist() {
        val cases = listOf(
            "http://auth.openai.com/oauth/authorize",
            "https://user@auth.openai.com/oauth/authorize",
            "https://auth.openai.com./oauth/authorize",
            "https://127.0.0.1/oauth/authorize",
            "https://[::1]/oauth/authorize",
            "https://localhost/oauth/authorize",
            "https://printer.local/oauth/authorize",
        )

        for (url in cases) {
            val decision = ExternalUrlPolicy.classifyExternalUrl(url, AuthUrlKind.AgentAuthorize)
            assertFalse("expected blocked: $url", decision.allowed)
            assertEquals("invalid_url", decision.reason)
            assertNull(decision.normalizedHost)
        }
    }

    @Test
    fun appOriginIsNeverAnOpenExternalTarget() {
        val decision = ExternalUrlPolicy.classifyExternalUrl(
            "https://${ExternalUrlPolicy.APP_HOST}/",
            AuthUrlKind.AgentAuthorize,
        )
        assertFalse(decision.allowed)
        assertEquals("not_allowlisted", decision.reason)
        assertEquals(ExternalUrlPolicy.APP_HOST, decision.normalizedHost)
    }

    @Test
    fun navigationClassificationSeparatesAppOriginExternalAuthAndBlockedUrls() {
        val appOrigin = ExternalUrlPolicy.classifyNavigationUrl("https://${ExternalUrlPolicy.APP_HOST}/index.html")
        assertEquals(NavigationAction.AppOrigin, appOrigin.action)
        assertEquals("app_origin", appOrigin.reason)

        val auth = ExternalUrlPolicy.classifyNavigationUrl("https://auth.openai.com/oauth/authorize")
        assertEquals(NavigationAction.ExternalBrowser, auth.action)
        assertEquals("allowed_auth", auth.reason)

        val deviceCode = ExternalUrlPolicy.classifyNavigationUrl("https://microsoft.com/devicelogin")
        assertEquals(NavigationAction.ExternalBrowser, deviceCode.action)
        assertEquals("allowed_auth", deviceCode.reason)

        val blocked = ExternalUrlPolicy.classifyNavigationUrl("https://example.com/")
        assertEquals(NavigationAction.Block, blocked.action)
        assertEquals("not_allowlisted", blocked.reason)
    }

    private fun assertAllowed(decision: ExternalUrlDecision, host: String) {
        assertTrue(decision.allowed)
        assertEquals("allowed", decision.reason)
        assertEquals(host, decision.normalizedHost)
    }

    private fun accountAuthorizeUrl(overrides: Map<String, String> = emptyMap()): String {
        val values = linkedMapOf(
            "client_id" to "a90d9140-3a62-46d0-907b-f2b7b61a573a",
            "response_type" to "code",
            "response_mode" to "query",
            "redirect_uri" to "http://localhost:45678",
            "scope" to accountScopes.joinToString(" "),
            "code_challenge" to "a".repeat(43),
            "code_challenge_method" to "S256",
            "state" to "b".repeat(43),
            "prompt" to "select_account",
        )
        values.putAll(overrides)
        val query = values.entries.joinToString("&") { (key, value) ->
            "${encode(key)}=${encode(value)}"
        }
        return "https://login.microsoftonline.com/consumers/oauth2/v2.0/authorize?$query"
    }

    private fun encode(value: String): String =
        URLEncoder.encode(value, StandardCharsets.UTF_8.name())

    private fun assertBlocked(decision: ExternalUrlDecision, reason: String) {
        assertFalse(decision.allowed)
        assertEquals(reason, decision.reason)
    }
}
