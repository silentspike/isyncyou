package com.silentspike.isyncyou

import android.app.Dialog
import android.text.method.PasswordTransformationMethod
import android.view.ContextThemeWrapper
import android.view.View
import android.view.WindowManager
import android.widget.CheckBox
import android.widget.LinearLayout
import android.widget.TextView
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.*
import org.junit.Test

class PushCredentialDialogInstrumentedTest {
    @Test fun passwordIsMaskedAndDialogIsSecure() {
        InstrumentationRegistry.getInstrumentation().runOnMainSync {
            val context = ContextThemeWrapper(
                InstrumentationRegistry.getInstrumentation().targetContext,
                android.R.style.Theme_Material_Dialog_Alert,
            )
            val content = pushCredentialContent(context, "https://example.invalid", "test-user", "synthetic-secret") as LinearLayout
            val password = content.getChildAt(1) as TextView
            val reveal = content.getChildAt(2) as CheckBox
            assertTrue(password.transformationMethod is PasswordTransformationMethod)
            assertEquals(View.IMPORTANT_FOR_ACCESSIBILITY_NO, password.importantForAccessibility)
            assertFalse(password.isTextSelectable)
            assertFalse((content.getChildAt(0) as TextView).text.contains("synthetic-secret"))
            reveal.isChecked = true
            assertNull(password.transformationMethod)
            assertEquals(View.IMPORTANT_FOR_ACCESSIBILITY_NO, password.importantForAccessibility)
            reveal.isChecked = false
            assertTrue(password.transformationMethod is PasswordTransformationMethod)
            val dialog = Dialog(context)
            securePushCredentialWindow(dialog)
            assertTrue(requireNotNull(dialog.window).attributes.flags and WindowManager.LayoutParams.FLAG_SECURE != 0)
        }
    }
}
