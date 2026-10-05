package dev.relay.desktop

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat
import java.io.File

/**
 * Keeps the process alive and runs the Relay engine while the UI is closed.
 *
 * The engine is the same in-process runner the UI uses (mobile.rs), so the
 * activity and this service never run two copies. Android 15 caps dataSync
 * foreground services at 6 h per 24 h; on timeout the service stops sync and
 * itself.
 */
class RelaySyncService : Service() {
  companion object {
    private const val CHANNEL_ID = "sync"
    private const val NOTIFICATION_ID = 1
    private const val ACTION_STOP = "dev.relay.desktop.STOP_SYNC"
    private const val STATUS_INTERVAL_MS = 5_000L

    fun start(context: Context) {
      ContextCompat.startForegroundService(context, Intent(context, RelaySyncService::class.java))
    }

    /** Must match Tauri's app_data_dir(): dataDir/<bundle identifier>. */
    fun home(context: Context): File = File(context.dataDir, context.packageName)
  }

  private val handler = Handler(Looper.getMainLooper())
  private var multicastLock: WifiManager.MulticastLock? = null
  private var lastStatus = ""

  private val refresh = object : Runnable {
    override fun run() {
      val status = RelayNative.status()
      if (status != lastStatus) {
        lastStatus = status
        getSystemService(NotificationManager::class.java).notify(NOTIFICATION_ID, notification(status))
      }
      handler.postDelayed(this, STATUS_INTERVAL_MS)
    }
  }

  override fun onBind(intent: Intent?): IBinder? = null

  override fun onCreate() {
    super.onCreate()
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
      val channel = NotificationChannel(CHANNEL_ID, "Sync", NotificationManager.IMPORTANCE_LOW)
      channel.setShowBadge(false)
      getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
    }
    val type = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
      ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC
    } else {
      0
    }
    ServiceCompat.startForeground(this, NOTIFICATION_ID, notification("Starting…"), type)

    // mDNS discovery needs multicast; Wi-Fi drops it without a lock.
    val wifi = applicationContext.getSystemService(WifiManager::class.java)
    multicastLock = wifi?.createMulticastLock("relay-mdns")?.apply {
      setReferenceCounted(false)
      acquire()
    }

    val home = home(this)
    home.mkdirs()
    RelayNative.start(home.absolutePath)
    handler.post(refresh)
  }

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    if (intent?.action == ACTION_STOP) {
      stopSelf()
      return START_NOT_STICKY
    }
    return START_STICKY
  }

  override fun onTimeout(startId: Int, fgsType: Int) {
    stopSelf()
  }

  override fun onDestroy() {
    handler.removeCallbacks(refresh)
    multicastLock?.release()
    multicastLock = null
    // stop() joins the engine thread for up to 5 s; keep it off the main thread.
    Thread({ RelayNative.stop() }, "relay-stop").start()
    super.onDestroy()
  }

  private fun notification(status: String): Notification {
    val open = PendingIntent.getActivity(
      this,
      0,
      Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
      PendingIntent.FLAG_IMMUTABLE,
    )
    val stop = PendingIntent.getService(
      this,
      1,
      Intent(this, RelaySyncService::class.java).setAction(ACTION_STOP),
      PendingIntent.FLAG_IMMUTABLE,
    )
    return NotificationCompat.Builder(this, CHANNEL_ID)
      .setSmallIcon(android.R.drawable.stat_notify_sync)
      .setContentTitle("Relay")
      .setContentText(status)
      .setContentIntent(open)
      .addAction(0, "Stop sync", stop)
      .setOngoing(true)
      .setOnlyAlertOnce(true)
      .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
      .build()
  }
}
