package com.silentspike.isyncyou

import android.content.Context
import android.util.Base64
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import com.google.crypto.tink.apps.fixed_webpush.WebPushHybridEncrypt
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import java.util.UUID

@RunWith(AndroidJUnit4::class)
class WebPushKeyManagerInstrumentedTest {
    @Test fun encryptedKeysSurviveRecreationAndDecryptWebPush() {
        val context = ApplicationProvider.getApplicationContext<Context>()
        val instance = "test_" + UUID.randomUUID().toString().replace("-", "")
        val manager = WebPushKeyManager(context)
        try {
            manager.generate(instance)
            val public = requireNotNull(manager.getPublicKeySet(instance))
            val auth = Base64.decode(public.auth, Base64.URL_SAFE or Base64.NO_PADDING or Base64.NO_WRAP)
            val point = Base64.decode(public.pubKey, Base64.URL_SAFE or Base64.NO_PADDING or Base64.NO_WRAP)
            val message = "verification challenge".toByteArray()
            val encrypted = WebPushHybridEncrypt.Builder().withAuthSecret(auth)
                .withRecipientPublicKey(point).build().encrypt(message, null)
            assertArrayEquals(message, WebPushKeyManager(context).decrypt(instance, encrypted))
            val stored = requireNotNull(context.getSharedPreferences("webpush_keys_v1", Context.MODE_PRIVATE)
                .getString(instance, null))
            assertFalse(stored.contains(public.auth))
            encrypted[encrypted.lastIndex] = (encrypted.last().toInt() xor 1).toByte()
            assertNull(manager.decrypt(instance, encrypted))
        } finally {
            manager.delete(instance)
        }
        assertFalse(manager.exists(instance))
    }
}
