package com.silentspike.isyncyou

/** Native framed router response validation before writing a user-selected document. */
internal object AttachmentDownloadPolicy {
    private const val MAX_BYTES = 64 * 1024 * 1024

    fun body(frame: ByteArray): ByteArray? {
        if (frame.size < 6 || frame.size > MAX_BYTES + 131076) return null
        fun u16(index: Int) = ((frame[index].toInt() and 255) shl 8) or (frame[index + 1].toInt() and 255)
        if (u16(0) != 200) return null
        val contentLength = u16(2)
        val headerPosition = 4 + contentLength
        if (headerPosition + 2 > frame.size) return null
        val contentType = String(frame, 4, contentLength, Charsets.UTF_8).substringBefore(';').trim()
        if (contentType != "application/octet-stream") return null
        val headerLength = u16(headerPosition)
        val offset = headerPosition + 2 + headerLength
        if (offset > frame.size || frame.size - offset > MAX_BYTES) return null
        val headers = String(frame, headerPosition + 2, headerLength, Charsets.UTF_8)
        val disposition = headers.split("\r\n").firstOrNull { it.startsWith("Content-Disposition:", ignoreCase = true) }
            ?: return null
        if (!disposition.substringAfter(':').substringBefore(';').trim().equals("attachment", ignoreCase = true)) return null
        return frame.copyOfRange(offset, frame.size)
    }

    fun filename(value: String): String {
        val safe = value.substringAfterLast('/').substringAfterLast('\\')
            .filterNot(Char::isISOControl).take(120).trim().trim('.')
        return safe.ifBlank { "attachment" }
    }
}
