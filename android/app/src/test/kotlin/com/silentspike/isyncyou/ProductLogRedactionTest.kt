package com.silentspike.isyncyou

import java.io.File
import kotlin.test.Test
import kotlin.test.assertFalse
import kotlin.test.assertNotNull

class ProductLogRedactionTest {
    @Test
    fun product_logs_do_not_pass_raw_throwables_or_dynamic_oauth_urls() {
        val sourceRoot = assertNotNull(
            sequenceOf(
                File("src/main/kotlin/com/silentspike/isyncyou"),
                File("app/src/main/kotlin/com/silentspike/isyncyou"),
            ).firstOrNull(File::isDirectory),
            "product Kotlin source root must exist",
        )

        val throwableLog = Regex(
            """(?:android\.util\.)?Log\.[a-z]+\([^\n]*,\s*(?:e|t|it)\s*\)""",
        )
        val sources = sourceRoot.walkTopDown().filter { it.isFile && it.extension == "kt" }
        sources.forEach { source ->
            val text = source.readText()
            assertFalse(
                throwableLog.containsMatchIn(text),
                "${source.name} must not pass a Throwable to product logging",
            )
        }

        val activity = File(sourceRoot, "MainActivity.kt").readText()
        assertFalse(activity.contains("shell loaded: \$url"))
        assertFalse(activity.contains("asset serve failed for \${url.encodedPath}"))
        assertFalse(activity.contains("external auth launch failed (\${decision.reason})"))
    }
}
