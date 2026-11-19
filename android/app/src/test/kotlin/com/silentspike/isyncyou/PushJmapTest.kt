package com.silentspike.isyncyou

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class PushJmapTest {
    @Test fun parsesOnlyCompleteUtcExpiry() {
        assertEquals(0L, requireNotNull(PushJmap.expiryMillis("1970-01-01T00:00:00Z")))
        assertTrue(PushJmap.expiryMillis("2030-01-01T00:00:00.123Z") != null)
        assertNull(PushJmap.expiryMillis("2030-01-01T00:00:00Zjunk"))
        assertNull(PushJmap.expiryMillis("2030-02-30T00:00:00Z"))
    }
}
