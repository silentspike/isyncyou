package com.silentspike.isyncyou;

import android.app.Activity;
import android.os.Bundle;
import android.view.KeyEvent;
import android.webkit.WebSettings;
import android.webkit.WebView;
import android.webkit.WebViewClient;

/**
 * iSyncYou Android client — a hardened WebView onto the local iSyncYou daemon's
 * web UI. For on-device testing the daemon is reached over `adb reverse`
 * (localhost:8869 -> host); a real deployment points SERVER_URL at the daemon's
 * reachable address (LAN IP / NetBird VPN).
 */
public class MainActivity extends Activity {

    /** Default daemon URL. Works on the USB-connected device via `adb reverse tcp:8869`. */
    private static final String SERVER_URL = "http://localhost:8869/";

    private WebView web;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);

        web = new WebView(this);
        WebSettings s = web.getSettings();
        s.setJavaScriptEnabled(true);
        s.setDomStorageEnabled(true);
        s.setLoadWithOverviewMode(true);
        s.setUseWideViewPort(false);
        // Always fetch the current UI from the daemon — the WebView was serving a
        // stale app.js/app.css; force no-cache so changes always show.
        s.setCacheMode(WebSettings.LOAD_NO_CACHE);
        web.clearCache(true);
        // keep navigation inside the WebView (don't hand off to Chrome)
        web.setWebViewClient(new WebViewClient());
        setContentView(web);

        if (savedInstanceState != null) {
            web.restoreState(savedInstanceState);
        } else {
            web.loadUrl(SERVER_URL);
        }
    }

    /** Hardware/gesture back navigates the WebView history before leaving the app. */
    @Override
    public boolean onKeyDown(int keyCode, KeyEvent event) {
        if (keyCode == KeyEvent.KEYCODE_BACK && web.canGoBack()) {
            web.goBack();
            return true;
        }
        return super.onKeyDown(keyCode, event);
    }

    @Override
    protected void onSaveInstanceState(Bundle outState) {
        super.onSaveInstanceState(outState);
        web.saveState(outState);
    }
}
