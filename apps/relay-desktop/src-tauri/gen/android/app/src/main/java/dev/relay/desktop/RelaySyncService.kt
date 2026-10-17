package dev.relay.desktop

import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.wifi.WifiManager
import android.os.Build
import android.os.IBinder
import androidx.core.app.ServiceCompat
import java.io.File

/**
 * Keeps the process alive while files are sent to a computer (D50), then
 * stops, so there is no notification while nothing is happening.
 *
 * A tap on "Send to …" or the share sheet starts it in the foreground with
 * the "Sending…" notification. The engine is the same in-process runner the
 * UI uses (mobile.rs); while the app is open it syncs without this service.
 * When the last send finishes, the service leaves the result notification
 * behind and stops. The engine keeps running until Android freezes or ends
 * the idle process.
 */
class RelaySyncService : Service() {
  companion object {
    /** The always-on status channel this service used to post to. */
    private const val OLD_SYNC_CHANNEL = "sync"

    /**
     * The Relay home. Must match app_home() in lib.rs: Tauri's
     * app_data_dir() on Android is dataDir itself, plus "relay".
     */
    fun home(context: Context): File = File(context.dataDir, "relay")

    /** A send is keeping the process in the foreground (SyncCatchUp checks). */
    @Volatile var active = false
  }

  private var multicastLock: WifiManager.MulticastLock? = null
  /** Sends in flight. Touched on the main thread only. */
  private var sending = 0

  override fun onBind(intent: Intent?): IBinder? = null

  override fun onCreate() {
    super.onCreate()
    active = true
    Drops.createChannels(this)
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
      getSystemService(NotificationManager::class.java).deleteNotificationChannel(OLD_SYNC_CHANNEL)
    }
    // mDNS discovery needs multicast; Wi-Fi drops it without a lock.
    val wifi = applicationContext.getSystemService(WifiManager::class.java)
    multicastLock = wifi?.createMulticastLock("relay-mdns")?.apply {
      setReferenceCounted(false)
      acquire()
    }
    val home = home(this)
    home.mkdirs()
    RelayNative.start(home.absolutePath)
  }

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    val peer = intent?.getStringExtra(Drops.EXTRA_PEER)
    if (intent?.action != Drops.ACTION_SEND || peer == null) {
      if (sending == 0) stopSelf()
      return START_NOT_STICKY
    }
    // A foreground start must show a notification within seconds: the
    // progress of this send is that notification.
    val count = intent.getStringArrayListExtra(Drops.EXTRA_URIS).orEmpty().size +
      intent.getStringArrayListExtra(Drops.EXTRA_PATHS).orEmpty().size
    val type = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
      ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC
    } else {
      0
    }
    ServiceCompat.startForeground(this, Drops.NOTIFY_SEND, Drops.progress(this, peer, count), type)
    sending++
    Drops.send(this, intent) {
      sending--
      if (sending == 0) {
        // Drops.send posts the result right after this.
        ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
        stopSelf()
      }
    }
    return START_NOT_STICKY
  }

  override fun onTimeout(startId: Int, fgsType: Int) {
    stopSelf()
  }

  override fun onDestroy() {
    active = false
    multicastLock?.release()
    multicastLock = null
    super.onDestroy()
  }
}
