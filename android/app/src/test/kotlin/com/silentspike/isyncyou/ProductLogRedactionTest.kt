package com.silentspike.isyncyou

import java.io.File
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Test

class ProductLogRedactionTest {
    @Test
    fun product_logs_do_not_pass_raw_throwables_or_dynamic_oauth_urls() {
        val sourceRoot = sequenceOf(
            File("src/main/kotlin/com/silentspike/isyncyou"),
            File("app/src/main/kotlin/com/silentspike/isyncyou"),
        ).firstOrNull(File::isDirectory)
        assertNotNull(
            "product Kotlin source root must exist",
            sourceRoot,
        )
        val sourceDir = checkNotNull(sourceRoot)

        val throwableLog = Regex(
            """(?:android\.util\.)?Log\.[a-z]+\([^\n]*,\s*(?:e|t|it)\s*\)""",
        )
        val sources = sourceDir.walkTopDown().filter { it.isFile && it.extension == "kt" }
        sources.forEach { source ->
            val text = source.readText()
            assertFalse(
                "${source.name} must not pass a Throwable to product logging",
                throwableLog.containsMatchIn(text),
            )
        }

        val activity = File(sourceDir, "MainActivity.kt").readText()
        assertFalse(activity.contains("shell loaded: \$url"))
        assertFalse(activity.contains("asset serve failed for \${url.encodedPath}"))
        assertFalse(activity.contains("external auth launch failed (\${decision.reason})"))
    }
}
