package com.silentspike.isyncyou

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class AttachmentDownloadPolicyTest {
    private fun frame(status: Int, mime: String, headers: String, body: ByteArray): ByteArray {
        val type = mime.toByteArray()
        val hdr = headers.toByteArray()
        fun n(value: Int) = byteArrayOf((value shr 8).toByte(), value.toByte())
        return n(status) + n(type.size) + type + n(hdr.size) + hdr + body
    }

    @Test fun acceptsOnlySuccessfulAttachmentFrames() {
        val body = byteArrayOf(0, 1, -1)
        val headers = "Content-Disposition: attachment; filename=\"file\"\r\n"
        assertArrayEquals(body, AttachmentDownloadPolicy.body(frame(200, "application/octet-stream", headers, body)))
        assertNull(AttachmentDownloadPolicy.body(frame(403, "application/octet-stream", headers, body)))
        assertNull(AttachmentDownloadPolicy.body(frame(200, "text/html", headers, body)))
        assertNull(AttachmentDownloadPolicy.body(frame(200, "application/octet-stream", "", body)))
        assertNull(AttachmentDownloadPolicy.body(frame(200, "application/octet-stream", "Content-Disposition: attachment-invalid\r\n", body)))
        assertNull(AttachmentDownloadPolicy.body(byteArrayOf(0, -56, 127, -1, 0, 0)))
    }

    @Test fun stripsPathAndControlCharactersFromSuggestedName() {
        assertEquals("name.txt", AttachmentDownloadPolicy.filename("../name.txt"))
        assertEquals("attachment", AttachmentDownloadPolicy.filename(".."))
        assertEquals("ab", AttachmentDownloadPolicy.filename("a\u0000b"))
    }
}
