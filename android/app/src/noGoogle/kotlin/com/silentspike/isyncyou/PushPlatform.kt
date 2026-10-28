package com.silentspike.isyncyou

import android.content.Context

internal object PushPlatform {
    fun initialize(context: Context) = Unit
    fun currentToken(context: Context): String = ""
}
