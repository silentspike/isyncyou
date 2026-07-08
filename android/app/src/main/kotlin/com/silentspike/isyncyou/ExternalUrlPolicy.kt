package com.silentspike.isyncyou

import java.net.IDN
import java.net.URI
import java.util.Locale

enum class AuthUrlKind(val wire: String) {
    AgentAuthorize("agent_authorize"),
    AccountDeviceCode("account_device_code"),
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
