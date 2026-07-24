package com.silentspike.isyncyou

import java.net.IDN
import java.net.URI
import java.net.URLDecoder
import java.nio.charset.StandardCharsets
import java.util.Locale

enum class AuthUrlKind(val wire: String) {
    AgentAuthorize("agent_authorize"),
    AccountDeviceCode("account_device_code"),
    AccountAuthorize("account_authorize"),
}

data class ExternalUrlDecision(
    val allowed: Boolean,
    val reason: String,
    val normalizedHost: String? = null,
)

data class NavigationDecision(
    val action: NavigationAction,
    val reason: String,
    val normalizedHost: String? = null,
)

enum class NavigationAction {
    AppOrigin,
    ExternalBrowser,
    Block,
}

object ExternalUrlPolicy {
    const val APP_HOST = "appassets.androidplatform.net"

    fun authKindFromWire(kind: String?): AuthUrlKind? = when (kind) {
        AuthUrlKind.AgentAuthorize.wire -> AuthUrlKind.AgentAuthorize
        AuthUrlKind.AccountDeviceCode.wire -> AuthUrlKind.AccountDeviceCode
        AuthUrlKind.AccountAuthorize.wire -> AuthUrlKind.AccountAuthorize
        else -> null
    }

    fun classifyExternalUrl(rawUrl: String?, kindWire: String?): ExternalUrlDecision {
        val kind = authKindFromWire(kindWire)
            ?: return ExternalUrlDecision(false, "missing_or_unknown_kind")
        return classifyExternalUrl(rawUrl, kind)
    }

    fun classifyExternalUrl(rawUrl: String?, kind: AuthUrlKind): ExternalUrlDecision {
        val normalized = normalizeHttpsUrl(rawUrl) ?: return ExternalUrlDecision(false, "invalid_url")
        val (uri, host) = normalized
        val path = uri.rawPath ?: "/"
        val allowed = when (kind) {
            AuthUrlKind.AgentAuthorize -> isAgentAuthorizeHostPath(host, path)
            AuthUrlKind.AccountDeviceCode -> isAccountDeviceCodeHostPath(host, path)
            AuthUrlKind.AccountAuthorize -> isAccountAuthorizeUrl(uri, host, path)
        }
        return if (allowed) {
            ExternalUrlDecision(true, "allowed", host)
        } else {
            ExternalUrlDecision(false, "not_allowlisted", host)
        }
    }

    fun classifyNavigationUrl(rawUrl: String?): NavigationDecision {
        val normalized = normalizeHttpsUrl(rawUrl) ?: return NavigationDecision(NavigationAction.Block, "invalid_url")
        val (uri, host) = normalized
        if (host == APP_HOST) {
            return NavigationDecision(NavigationAction.AppOrigin, "app_origin", host)
        }
        val path = uri.rawPath ?: "/"
        val allowed = isAgentAuthorizeHostPath(host, path) || isAccountDeviceCodeHostPath(host, path)
        return if (allowed) {
            NavigationDecision(NavigationAction.ExternalBrowser, "allowed_auth", host)
        } else {
            NavigationDecision(NavigationAction.Block, "not_allowlisted", host)
        }
    }

    private fun normalizeHttpsUrl(rawUrl: String?): Pair<URI, String>? {
        if (rawUrl.isNullOrBlank()) return null
        val uri = try {
            URI(rawUrl)
        } catch (_: Exception) {
            return null
        }
        if (uri.scheme?.lowercase(Locale.ROOT) != "https") return null
        if (uri.rawUserInfo != null) return null
        val rawHost = uri.host ?: return null
        if (rawHost.endsWith(".")) return null
        if (rawHost.any { it.code > 0x7f }) return null
        val host = try {
            IDN.toASCII(rawHost, IDN.USE_STD3_ASCII_RULES).lowercase(Locale.ROOT)
        } catch (_: Exception) {
            return null
        }
        if (host != rawHost.lowercase(Locale.ROOT)) return null
        if (isIpLiteral(host) || isInternalHost(host)) return null
        return uri to host
    }

    private fun isAgentAuthorizeHostPath(host: String, path: String): Boolean = when (host) {
        "claude.com" -> path == "/cai/oauth/authorize"
        "auth.openai.com" -> path == "/oauth/authorize"
        "login.microsoftonline.com" ->
            path == "/consumers/oauth2/v2.0/authorize" || path == "/common/oauth2/v2.0/authorize"
        "login.live.com" -> path == "/oauth20_authorize.srf"
        else -> false
    }

    private fun isAccountDeviceCodeHostPath(host: String, path: String): Boolean = when (host) {
        "microsoft.com" -> path == "/devicelogin"
        "www.microsoft.com" -> path == "/link"
        "login.microsoftonline.com" -> path == "/common/oauth2/deviceauth" || path == "/consumers/oauth2/deviceauth"
        "login.live.com" -> path == "/oauth20_remoteconnect.srf"
        else -> false
    }

    private fun isAccountAuthorizeUrl(uri: URI, host: String, path: String): Boolean {
        if (host != "login.microsoftonline.com" ||
            path != "/consumers/oauth2/v2.0/authorize" ||
            uri.port != -1 ||
            uri.rawFragment != null
        ) {
            return false
        }
        val query = decodeUniqueQuery(uri.rawQuery ?: return false) ?: return false
        if (query.keys != ACCOUNT_AUTHORIZE_QUERY_KEYS) return false
        val expectedScopes = when (query["client_id"]) {
            ACCOUNT_READ_CLIENT_ID -> ACCOUNT_READ_SCOPES
            ACCOUNT_WRITE_CLIENT_ID -> ACCOUNT_WRITE_SCOPES
            else -> return false
        }
        if (query["response_type"] != "code" ||
            query["response_mode"] != "query" ||
            query["code_challenge_method"] != "S256" ||
            query["prompt"] != "select_account" ||
            query["scope"]?.split(' ')?.toSet() != expectedScopes
        ) {
            return false
        }
        if (!isBase64UrlNonce(query["state"]) || !isBase64UrlNonce(query["code_challenge"])) {
            return false
        }
        return isExactLoopbackRedirect(query["redirect_uri"])
    }

    private fun decodeUniqueQuery(rawQuery: String): Map<String, String>? {
        val result = linkedMapOf<String, String>()
        for (part in rawQuery.split('&')) {
            val separator = part.indexOf('=')
            if (separator <= 0) return null
            val key = decodeQueryPart(part.substring(0, separator)) ?: return null
            val value = decodeQueryPart(part.substring(separator + 1)) ?: return null
            if (result.put(key, value) != null) return null
        }
        return result
    }

    private fun decodeQueryPart(value: String): String? = try {
        URLDecoder.decode(value, StandardCharsets.UTF_8.name())
    } catch (_: IllegalArgumentException) {
        null
    }

    private fun isBase64UrlNonce(value: String?): Boolean {
        if (value == null || value.length != 43) return false
        return value.all { it.isLetterOrDigit() || it == '-' || it == '_' }
    }

    private fun isExactLoopbackRedirect(value: String?): Boolean {
        val redirect = try {
            URI(value ?: return false)
        } catch (_: Exception) {
            return false
        }
        return redirect.scheme == "http" &&
            redirect.host == "localhost" &&
            redirect.port in 1..65535 &&
            redirect.rawPath.isEmpty() &&
            redirect.rawUserInfo == null &&
            redirect.rawQuery == null &&
            redirect.rawFragment == null
    }

    private const val ACCOUNT_READ_CLIENT_ID = "cee80dd9-c13e-4dbb-9d4c-73eb4987d447"
    private const val ACCOUNT_WRITE_CLIENT_ID = "a90d9140-3a62-46d0-907b-f2b7b61a573a"
    private val ACCOUNT_READ_SCOPES = setOf(
        "Files.Read",
        "Mail.Read",
        "MailboxSettings.Read",
        "Calendars.Read",
        "Contacts.Read",
        "Tasks.Read",
        "Notes.Read",
        "People.Read",
        "User.Read",
        "offline_access",
    )
    private val ACCOUNT_WRITE_SCOPES = setOf(
        "Files.ReadWrite",
        "Mail.ReadWrite",
        "Mail.Send",
        "MailboxSettings.ReadWrite",
        "Calendars.ReadWrite",
        "Contacts.ReadWrite",
        "Tasks.ReadWrite",
        "Notes.ReadWrite",
        "User.Read",
        "offline_access",
    )
    private val ACCOUNT_AUTHORIZE_QUERY_KEYS = setOf(
        "client_id",
        "response_type",
        "response_mode",
        "redirect_uri",
        "scope",
        "code_challenge",
        "code_challenge_method",
        "state",
        "prompt",
    )

    private fun isInternalHost(host: String): Boolean =
        host == "localhost" ||
            host.endsWith(".local") ||
            host.endsWith(".internal") ||
            host.endsWith(".lan") ||
            host.endsWith(".home") ||
            !host.contains('.')

    private fun isIpLiteral(host: String): Boolean =
        host.contains(':') || isIpv4Literal(host)

    private fun isIpv4Literal(host: String): Boolean {
        val parts = host.split('.')
        if (parts.size != 4) return false
        return parts.all { part ->
            part.isNotEmpty() && part.length <= 3 && part.all(Char::isDigit) && part.toIntOrNull() in 0..255
        }
    }
}
