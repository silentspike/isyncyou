package com.silentspike.isyncyou

import android.app.Activity
import android.os.Bundle
import android.view.KeyEvent
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient

/**
 * iSyncYou Android client — a hardened WebView onto the local iSyncYou daemon's
 * web UI. For on-device testing the daemon is reached over `adb reverse`
 * (localhost:8869 -> host); a real deployment points [SERVER_URL] at the daemon's
 * reachable address (LAN IP / NetBird VPN). A thin shell: all features live in the
 * web UI, so this stays small.
 */
class MainActivity : Activity() {

    private lateinit var web: WebView

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        web = WebView(this)
        web.settings.apply {
            javaScriptEnabled = true
            domStorageEnabled = true
            loadWithOverviewMode = true
            useWideViewPort = false
            // Always fetch the current UI from the daemon — avoid serving a stale
            // app.js/app.css so changes always show (#79).
            cacheMode = WebSettings.LOAD_NO_CACHE
        }
        web.clearCache(true)
        // Keep navigation inside the WebView (don't hand off to Chrome).
        web.webViewClient = WebViewClient()
        setContentView(web)

        if (savedInstanceState != null) {
            web.restoreState(savedInstanceState)
        } else {
            web.loadUrl(SERVER_URL)
        }
    }

    /** Hardware/gesture back navigates WebView history before leaving the app. */
    override fun onKeyDown(keyCode: Int, event: KeyEvent?): Boolean {
        if (keyCode == KeyEvent.KEYCODE_BACK && web.canGoBack()) {
            web.goBack()
            return true
        }
        return super.onKeyDown(keyCode, event)
    }

    override fun onSaveInstanceState(outState: Bundle) {
        super.onSaveInstanceState(outState)
        web.saveState(outState)
    }

    companion object {
        /** Default daemon URL. Works on a USB device via `adb reverse tcp:8869`. */
        private const val SERVER_URL = "http://localhost:8869/"
    }
}
