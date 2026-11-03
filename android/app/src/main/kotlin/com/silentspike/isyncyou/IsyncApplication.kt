package com.silentspike.isyncyou

import android.app.Application

class IsyncApplication : Application() {
    override fun onCreate() {
        super.onCreate()
        // Recovery remains unavailable if native registration fails. Other
        // startup paths retain their existing error handling.
        runCatching { NativeEngine.nativeInstallRecoverySigner(RecoveryKeyBridge(this)) }
    }
}
