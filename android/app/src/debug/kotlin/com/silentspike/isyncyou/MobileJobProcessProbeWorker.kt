package com.silentspike.isyncyou

import android.app.ActivityManager
import android.app.Application
import android.content.Context
import android.os.Build
import android.os.Process
import androidx.work.Worker
import androidx.work.WorkerParameters
import androidx.work.workDataOf

/** Debug-only instrumentation probe for the single-process mobile-job contract. */
class MobileJobProcessProbeWorker(
    context: Context,
    params: WorkerParameters,
) : Worker(context, params) {
    override fun doWork(): Result = Result.success(
        workDataOf(
            "pid" to Process.myPid(),
            "process_name" to currentProcessName(applicationContext),
        ),
    )
}

internal fun currentProcessName(context: Context): String {
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
        return Application.getProcessName()
    }
    val manager = context.getSystemService(Context.ACTIVITY_SERVICE) as ActivityManager
    return manager.runningAppProcesses
        ?.firstOrNull { it.pid == Process.myPid() }
        ?.processName
        .orEmpty()
}
