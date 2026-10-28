package com.silentspike.isyncyou

import android.content.Context
import android.util.Base64
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import java.security.KeyFactory
import java.security.Signature
import java.security.spec.X509EncodedKeySpec
import java.util.UUID

@RunWith(AndroidJUnit4::class)
class PushInstallationKeyInstrumentedTest {
    @Test fun stableInstallationSignsWithKeystoreKey() {
        val context = ApplicationProvider.getApplicationContext<Context>()
        val account = "test_" + UUID.randomUUID().toString().replace("-", "")
        try {
            val identity = PushInstallationKey.getOrCreate(context, account)
            assertEquals(identity, PushInstallationKey.getOrCreate(context, account))
            val flags = Base64.URL_SAFE or Base64.NO_PADDING or Base64.NO_WRAP
            val pub = KeyFactory.getInstance("EC").generatePublic(X509EncodedKeySpec(Base64.decode(identity.publicKey, flags)))
            val challenge = "one-use challenge".toByteArray()
            val signature = Base64.decode(PushInstallationKey.sign(context, account, challenge), flags)
            assertTrue(Signature.getInstance("SHA256withECDSA").apply {
                initVerify(pub); update(challenge)
            }.verify(signature))
            assertFalse(Signature.getInstance("SHA256withECDSA").apply {
                initVerify(pub); update("different challenge".toByteArray())
            }.verify(signature))
        } finally {
            PushInstallationKey.deleteAfterRevoke(context, account)
        }
    }
}
