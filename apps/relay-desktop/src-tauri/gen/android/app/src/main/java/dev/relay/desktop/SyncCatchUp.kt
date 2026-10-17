package dev.relay.desktop

import android.app.job.JobInfo
import android.app.job.JobParameters
import android.app.job.JobScheduler
import android.app.job.JobService
import android.content.ComponentName
import android.content.Context
import android.net.wifi.WifiManager
import android.os.SystemClock

/**
 * Background folder sync while Relay is closed, with no notification.
 *
 * A periodic JobScheduler job (Android's floor is 15 min) wakes the process,
 * runs the in-process engine until it has been quiet for a while or the
 * budget runs out, then stops it again if this job was the one that started
 * it. The job is persisted, so it survives a reboot without a boot receiver
 * starting a foreground service (which Android 15 forbids for dataSync).
 */
object SyncCatchUp {
  private const val JOB_ID = 1002
  private const val PERIOD_MS = 15 * 60 * 1000L

  /** Engines get this long to reconnect before quiet counts. */
  const val MIN_RUN_MS = 45_000L
  /** Stop once nothing has moved for this long. */
  const val QUIET_MS = 20_000L
  /** Regular jobs get about 10 min; leave room to stop cleanly. */
  const val BUDGET_MS = 8 * 60 * 1000L

  /** MainActivity is on screen; its sync must not be stopped under it. */
  @Volatile var uiVisible = false

  fun schedule(context: Context) {
    val scheduler = context.getSystemService(JobScheduler::class.java)
    if (scheduler.getPendingJob(JOB_ID) != null) return
    val job = JobInfo.Builder(JOB_ID, ComponentName(context, SyncCatchUpJob::class.java))
      .setPeriodic(PERIOD_MS)
      .setRequiredNetworkType(JobInfo.NETWORK_TYPE_ANY)
      .setRequiresBatteryNotLow(true)
      .setPersisted(true)
      .build()
    scheduler.schedule(job)
  }
}

class SyncCatchUpJob : JobService() {
  @Volatile private var cancelled = false

  override fun onStartJob(params: JobParameters): Boolean {
    cancelled = false
    Thread({
      try {
        run()
      } finally {
        jobFinished(params, false)
      }
    }, "relay-catch-up").start()
    return true
  }

  override fun onStopJob(params: JobParameters): Boolean {
    cancelled = true
    return false
  }

  private fun run() {
    val wifi = applicationContext.getSystemService(WifiManager::class.java)
    val lock = wifi?.createMulticastLock("relay-catch-up")?.apply {
      setReferenceCounted(false)
      acquire()
    }
    try {
      // The engine may still be alive from the last time the app was open
      // (the process was only frozen). Then it resumes on its own; this job
      // just keeps the process awake and leaves it running afterwards.
      val startedHere = !RelayNative.running()
      val home = RelaySyncService.home(this)
      home.mkdirs()
      RelayNative.start(home.absolutePath)
      // Not set up yet (no device): nothing to sync.
      if (!RelayNative.running()) return

      val began = SystemClock.elapsedRealtime()
      var lastBusy = began
      while (!cancelled) {
        val now = SystemClock.elapsedRealtime()
        if (RelayNative.busy()) lastBusy = now
        val ran = now - began
        if (ran >= SyncCatchUp.BUDGET_MS) break
        if (ran >= SyncCatchUp.MIN_RUN_MS && now - lastBusy >= SyncCatchUp.QUIET_MS) break
        Thread.sleep(2_000)
      }
      if (startedHere && !SyncCatchUp.uiVisible && !RelaySyncService.active) {
        RelayNative.stop()
      }
    } finally {
      lock?.release()
    }
  }
}
