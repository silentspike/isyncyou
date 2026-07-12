package com.silentspike.isyncyou

import android.content.Context
import android.os.Handler
import android.os.Looper

/** Process/application-owned guard runtime. Activity recreation never clears leases. */
object NetworkCriticalGuardRuntime {
    private val lock = Any()
    private val handler = Handler(Looper.getMainLooper())
    private var appContext: Context? = null
    private var registry: NetworkCriticalGuardRegistry? = null
    private var scheduledExpiry: Runnable? = null

    fun initialize(context: Context) {
        synchronized(lock) {
            if (registry != null) return
            appContext = context.applicationContext
            registry = NetworkCriticalGuardRegistry(
                onStart = { NetworkCriticalGuardService.start(requireContext()) },
                onStop = { NetworkCriticalGuardService.stop(requireContext()) },
                nowElapsedMs = { android.os.SystemClock.elapsedRealtime() },
                onInvalidate = { ids -> ids.forEach(NativeEngine::nativeInvalidateNetworkGuard) },
            )
        }
    }

    fun begin(reason: NetworkGuardReason): NetworkGuardBeginResult = synchronized(lock) {
        val result = requireRegistry().begin(reason)
        scheduleExpiryLocked()
        result
    }

    fun bindTurn(guardId: String?, turnId: String?): Boolean = synchronized(lock) {
        val bound = requireRegistry().bindTurn(guardId, turnId)
        scheduleExpiryLocked()
        bound
    }

    fun end(guardId: String?): Boolean = synchronized(lock) {
        val ended = requireRegistry().end(guardId)
        scheduleExpiryLocked()
        ended
    }

    fun activeLease(guardId: String?): NetworkGuardLeaseSnapshot? = synchronized(lock) {
        val lease = requireRegistry().activeLease(guardId)
        scheduleExpiryLocked()
        lease
    }

    fun onServiceDestroyed() {
        synchronized(lock) {
            requireRegistry().invalidateAll()
            scheduledExpiry?.let(handler::removeCallbacks)
            scheduledExpiry = null
        }
    }

    internal fun activeCountForTests(): Int = synchronized(lock) { requireRegistry().activeCount() }

    private fun requireContext(): Context = checkNotNull(appContext) { "network guard not initialized" }

    private fun requireRegistry(): NetworkCriticalGuardRegistry =
        checkNotNull(registry) { "network guard not initialized" }

    private fun scheduleExpiryLocked() {
        scheduledExpiry?.let(handler::removeCallbacks)
        scheduledExpiry = null
        val next = requireRegistry().nextDeadlineElapsedMs() ?: return
        val now = android.os.SystemClock.elapsedRealtime()
        val delay = (next - now).coerceAtLeast(0L)
        val task = Runnable {
            synchronized(lock) {
                requireRegistry().reapExpired()
                scheduleExpiryLocked()
            }
        }
        scheduledExpiry = task
        handler.postDelayed(task, delay)
    }
}
