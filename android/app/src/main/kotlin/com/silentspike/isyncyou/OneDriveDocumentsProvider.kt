package com.silentspike.isyncyou

import android.database.Cursor
import android.database.MatrixCursor
import android.os.Build
import android.os.CancellationSignal
import android.os.Handler
import android.os.HandlerThread
import android.os.ParcelFileDescriptor
import android.os.ProxyFileDescriptorCallback
import android.os.storage.StorageManager
import android.provider.DocumentsContract.Document
import android.provider.DocumentsContract.Root
import android.provider.DocumentsProvider
import android.webkit.MimeTypeMap
import androidx.annotation.RequiresApi
import java.io.FileNotFoundException
import java.net.URLDecoder
import java.net.URLEncoder
import java.text.SimpleDateFormat
import java.util.Arrays
import java.util.Locale
import java.util.TimeZone
import org.json.JSONArray
import org.json.JSONObject

/**
 * Read-only Storage Access Framework provider (S-OM.12, #658): surfaces the "me" OneDrive account in
 * the Android Documents UI, backed by the embedded engine over the **existing** JNI bridge.
 *
 * Live Mode-1 browsing (#648 / #649): [NativeEngine.nativeBridgeRequest] walks the folder tree straight
 * from Graph (`/api/v1/onedrive/children`, fully paged, no store write) and
 * [NativeEngine.nativeAssetRequestWithSession] fetches file bytes on demand
 * (`/api/v1/onedrive/open`, a live Graph download). Reads only,
 * session-token gated (never biometric).
 *
 * **No decrypted plaintext ever touches disk:** bytes are served from RAM via a proxy FD (API 26+) or a
 * pipe (24-25 fallback — never a temp file), and zeroed on release.
 *
 * Mode 1 keeps no store metadata, so there is no single-item lookup; document ids are **self-contained**
 * (`urlenc(graphId)|0|1|urlenc(name)`) so queryDocument / openDocument need no extra call.
 */
class OneDriveDocumentsProvider : DocumentsProvider() {

    override fun onCreate(): Boolean = true

    // ---------------------------------------------------------------- roots

    override fun queryRoots(projection: Array<String>?): Cursor {
        val proj = projection ?: DEFAULT_ROOT_PROJECTION
        val c = MatrixCursor(proj)
        // Always advertise the single OneDrive root (shows even before sign-in); children load lazily.
        val row = c.newRow()
        fun put(col: String, v: Any?) { if (col in proj) row.add(col, v) }
        put(Root.COLUMN_ROOT_ID, ROOT_ID)
        put(Root.COLUMN_DOCUMENT_ID, ROOT_DOC_ID)
        put(Root.COLUMN_TITLE, "OneDrive")
        put(Root.COLUMN_FLAGS, Root.FLAG_SUPPORTS_IS_CHILD) // read-only: no CREATE
        put(Root.COLUMN_ICON, R.mipmap.ic_launcher)
        put(Root.COLUMN_SUMMARY, ACCOUNT)
        return c
    }

    // ---------------------------------------------------------------- documents

    override fun queryDocument(documentId: String, projection: Array<String>?): Cursor {
        val proj = projection ?: DEFAULT_DOCUMENT_PROJECTION
        val c = MatrixCursor(proj)
        val row = c.newRow()
        fun put(col: String, v: Any?) { if (col in proj) row.add(col, v) }
        if (documentId == ROOT_DOC_ID) {
            put(Document.COLUMN_DOCUMENT_ID, ROOT_DOC_ID)
            put(Document.COLUMN_DISPLAY_NAME, "OneDrive")
            put(Document.COLUMN_MIME_TYPE, Document.MIME_TYPE_DIR)
            put(Document.COLUMN_FLAGS, 0)
            return c
        }
        // Self-contained id — no server round-trip (Mode 1 has no store metadata to query).
        val d = DocId.decode(documentId)
        put(Document.COLUMN_DOCUMENT_ID, documentId)
        put(Document.COLUMN_DISPLAY_NAME, d.name)
        put(Document.COLUMN_MIME_TYPE, if (d.isFolder) Document.MIME_TYPE_DIR else mimeOf(d.name, null))
        put(Document.COLUMN_FLAGS, 0)
        return c
    }

    override fun queryChildDocuments(
        parentDocumentId: String,
        projection: Array<String>?,
        sortOrder: String?,
    ): Cursor {
        val proj = projection ?: DEFAULT_DOCUMENT_PROJECTION
        val c = MatrixCursor(proj)
        // The drive root is the empty folder id; a subfolder is the graph id inside its document id.
        val folder = if (parentDocumentId == ROOT_DOC_ID) "" else DocId.decode(parentDocumentId).id
        val children = try {
            liveChildren(folder)
        } catch (e: EncryptedStorageSetupException) {
            throw encryptedStorageUnavailable(e)
        } catch (e: IllegalStateException) {
            throw encryptedStorageUnavailable(e)
        }
        for (i in 0 until children.length()) {
            children.optJSONObject(i)?.let { addChildRow(c, proj, it) }
        }
        return c
    }

    override fun openDocument(
        documentId: String,
        mode: String,
        signal: CancellationSignal?,
    ): ParcelFileDescriptor {
        if (mode != "r") throw FileNotFoundException("read-only provider; unsupported mode: $mode")
        val d = DocId.decode(documentId)
        val (status, body) = try {
            liveOpen(d.id, d.name)
        } catch (e: EncryptedStorageSetupException) {
            throw encryptedStorageUnavailable(e)
        } catch (e: IllegalStateException) {
            throw encryptedStorageUnavailable(e)
        }
        if (status != 200 || body == null) {
            throw FileNotFoundException("document not available (status=$status): ${d.id}")
        }
        return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) proxyFd(body) else pipeFd(body)
    }

    // -------------------------------------------------- FD delivery (proxy FD, never a temp file)

    @RequiresApi(Build.VERSION_CODES.O)
    private fun proxyFd(bytes: ByteArray): ParcelFileDescriptor {
        val sm = context!!.getSystemService(StorageManager::class.java)
        val ht = HandlerThread("onedrive-pfd").apply { start() }
        val cb = object : ProxyFileDescriptorCallback() {
            override fun onGetSize(): Long = bytes.size.toLong()
            override fun onRead(offset: Long, size: Int, data: ByteArray): Int {
                if (offset >= bytes.size) return 0
                val len = minOf(size.toLong(), bytes.size - offset).toInt()
                System.arraycopy(bytes, offset.toInt(), data, 0, len)
                return len
            }
            override fun onRelease() {
                Arrays.fill(bytes, 0) // wipe the decrypted plaintext from RAM (#0B hygiene)
                ht.quitSafely()
            }
        }
        return sm.openProxyFileDescriptor(ParcelFileDescriptor.MODE_READ_ONLY, cb, Handler(ht.looper))
    }

    private fun pipeFd(bytes: ByteArray): ParcelFileDescriptor {
        // API 24-25 fallback: a pipe (not seekable) — still no plaintext on disk (never a temp file).
        val pipe = ParcelFileDescriptor.createReliablePipe()
        Thread {
            ParcelFileDescriptor.AutoCloseOutputStream(pipe[1]).use { out ->
                try {
                    out.write(bytes)
                } catch (_: Exception) {
                    // reader closed early — nothing to do
                } finally {
                    Arrays.fill(bytes, 0)
                }
            }
        }.start()
        return pipe[0]
    }

    // -------------------------------------------------- engine bridge helpers (live Mode-1)

    private fun encryptedStorageUnavailable(cause: Exception): FileNotFoundException {
        return FileNotFoundException("encrypted local engine unavailable").apply {
            initCause(cause)
        }
    }

    private fun token(): String {
        val t = EngineBootstrap.ensureStarted(context!!.filesDir)
        if (t.isEmpty()) throw IllegalStateException("encrypted local engine unavailable")
        return t
    }

    /**
     * `GET /api/v1/onedrive/children?folder=` → the live Graph child list.
     * Remote/API failures stay an empty cloud listing, but local encrypted-storage/bootstrap
     * failures must propagate so the provider fails closed instead of presenting usable state.
     */
    private fun liveChildren(folder: String): JSONArray {
        val reply = bridgeGet("/api/v1/onedrive/children?account=$ACCOUNT&folder=${enc(folder)}")
            ?: return JSONArray()
        return try {
            if (reply.getInt("status") != 200) JSONArray()
            else JSONObject(reply.optString("body")).optJSONArray("children") ?: JSONArray()
        } catch (e: Exception) {
            JSONArray()
        }
    }

    /**
     * Send a bridge GET with the session token.
     * Returns null for malformed/remote bridge replies, but never hides local engine availability
     * failures that would make encrypted local state unsafe to use.
     */
    private fun bridgeGet(path: String): JSONObject? {
        return try {
            val env = JSONObject().apply {
                put("t", "req")
                put("id", "1")
                put("method", "GET")
                put("path", path)
                put("headers", JSONObject().apply { put("X-Session-Token", token()) })
            }
            JSONObject(NativeEngine.nativeBridgeRequest(env.toString()))
        } catch (e: EncryptedStorageSetupException) {
            throw e
        } catch (e: IllegalStateException) {
            throw e
        } catch (e: Exception) {
            null
        }
    }

    /** `GET /api/v1/onedrive/open?id=&name=` over the binary-safe asset path → (status, bytes-or-null). */
    private fun liveOpen(id: String, name: String): Pair<Int, ByteArray?> {
        val path = "/api/v1/onedrive/open?account=$ACCOUNT&id=${enc(id)}&name=${enc(name)}"
        return parseAssetFrame(NativeEngine.nativeAssetRequestWithSession(path, token()))
    }

    /**
     * Decode the asset frame `[status:u16 BE][ct_len:u16 BE][ct][hdr_len:u16 BE][headers][body]`
     * (verified against Rust `asset_request` + MainActivity.decodeAssetResponse) → (status, body).
     */
    private fun parseAssetFrame(framed: ByteArray): Pair<Int, ByteArray?> {
        if (framed.size < 6) return Pair(503, null)
        fun u16(i: Int) = ((framed[i].toInt() and 0xff) shl 8) or (framed[i + 1].toInt() and 0xff)
        val status = u16(0)
        val ctLen = u16(2)
        val hdrOff = 4 + ctLen
        if (hdrOff + 2 > framed.size) return Pair(status, null)
        val hdrLen = u16(hdrOff)
        val bodyOff = hdrOff + 2 + hdrLen
        if (bodyOff > framed.size) return Pair(status, null)
        return Pair(status, framed.copyOfRange(bodyOff, framed.size))
    }

    // -------------------------------------------------- row building (raw Graph DriveItem)

    private fun addChildRow(c: MatrixCursor, proj: Array<String>, item: JSONObject) {
        val id = item.optString("id")
        if (id.isEmpty()) return
        val name = item.optString("name", id)
        // Graph marks a folder with a `folder` facet and a file with a `file` facet.
        val isFolder = item.has("folder") && !item.isNull("folder")
        val row = c.newRow()
        fun put(col: String, v: Any?) { if (col in proj) row.add(col, v) }
        put(Document.COLUMN_DOCUMENT_ID, DocId.encode(id, name, isFolder))
        put(Document.COLUMN_DISPLAY_NAME, name)
        put(Document.COLUMN_MIME_TYPE, if (isFolder) Document.MIME_TYPE_DIR else mimeOf(name, item))
        put(Document.COLUMN_FLAGS, 0) // read-only: no WRITE/DELETE/RENAME
        if (item.has("size") && !item.isNull("size")) put(Document.COLUMN_SIZE, item.optLong("size"))
        parseRfc3339(item.optString("lastModifiedDateTime"))?.let { put(Document.COLUMN_LAST_MODIFIED, it) }
    }

    /** MIME from the Graph `file` facet if present, else derived from the name's extension. */
    private fun mimeOf(name: String, item: JSONObject?): String {
        item?.optJSONObject("file")?.optString("mimeType")?.takeIf { it.isNotEmpty() }?.let { return it }
        val ext = name.substringAfterLast('.', "").lowercase(Locale.US)
        return MimeTypeMap.getSingleton().getMimeTypeFromExtension(ext) ?: "application/octet-stream"
    }

    /** RFC3339 (e.g. `2024-01-02T03:04:05Z` / `…05.123Z`) → epoch millis, or null. */
    private fun parseRfc3339(s: String?): Long? {
        if (s.isNullOrEmpty()) return null
        return try {
            val core = s.substringBefore('.').removeSuffix("Z")
            val f = SimpleDateFormat("yyyy-MM-dd'T'HH:mm:ss", Locale.US)
                .apply { timeZone = TimeZone.getTimeZone("UTC") }
            f.parse(core)?.time
        } catch (e: Exception) {
            null
        }
    }

    private fun enc(s: String): String = URLEncoder.encode(s, "UTF-8")

    /**
     * A self-contained document id `urlenc(graphId)|0/1|urlenc(name)`. `|` is a safe separator because
     * URL-encoding never emits it. Carries the graph id (for children/open) plus the name+kind (for
     * queryDocument display + the open content-type) so Mode-1 (no store metadata) needs no lookup.
     */
    private class DocId(val id: String, val name: String, val isFolder: Boolean) {
        companion object {
            fun encode(id: String, name: String, isFolder: Boolean): String =
                "${e(id)}|${if (isFolder) 1 else 0}|${e(name)}"

            fun decode(documentId: String): DocId {
                val p = documentId.split("|")
                if (p.size < 3) return DocId(documentId, documentId, false)
                return DocId(d(p[0]), d(p[2]), p[1] == "1")
            }

            private fun e(s: String) = URLEncoder.encode(s, "UTF-8")
            private fun d(s: String) = URLDecoder.decode(s, "UTF-8")
        }
    }

    companion object {
        private const val ACCOUNT = "me"
        private const val ROOT_ID = "onedrive"
        private const val ROOT_DOC_ID = "root"

        private val DEFAULT_ROOT_PROJECTION = arrayOf(
            Root.COLUMN_ROOT_ID,
            Root.COLUMN_DOCUMENT_ID,
            Root.COLUMN_TITLE,
            Root.COLUMN_FLAGS,
            Root.COLUMN_ICON,
            Root.COLUMN_SUMMARY,
        )
        private val DEFAULT_DOCUMENT_PROJECTION = arrayOf(
            Document.COLUMN_DOCUMENT_ID,
            Document.COLUMN_DISPLAY_NAME,
            Document.COLUMN_MIME_TYPE,
            Document.COLUMN_SIZE,
            Document.COLUMN_FLAGS,
            Document.COLUMN_LAST_MODIFIED,
        )
    }
}
