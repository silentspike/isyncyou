package com.silentspike.isyncyou

import android.content.Context
import com.google.firebase.FirebaseApp
import com.google.firebase.messaging.FirebaseMessaging

internal object PushPlatform {
    fun initialize(context: Context) {
        IsyncMessagingService.ensureChannel(context)
        if (FirebaseApp.getApps(context).isNotEmpty()) {
            FirebaseMessaging.getInstance().token.addOnCompleteListener { result ->
                if (result.isSuccessful) IsyncMessagingService.saveToken(context, result.result)
            }
        }
    }

    fun currentToken(context: Context): String = IsyncMessagingService.currentToken(context)
}
