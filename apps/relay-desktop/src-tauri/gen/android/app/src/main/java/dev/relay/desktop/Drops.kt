package dev.relay.desktop

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.provider.OpenableColumns
import android.util.Log
import androidx.core.app.NotificationCompat
import java.io.File

/**
 * Sending files to a computer (D50, docs/proposals/photo-drop.md).
 *
 * The engine reads plain paths, so each file is first copied into
 * cacheDir/drops and deleted once it is sent. A send is started through
 * [RelaySyncService], which keeps the process alive and runs sync.
 */
object Drops {
  private const val TAG = "RelayDrops"

  /** "Send to …" after a new photo. Heads-up, no sound. */
  const val CHANNEL_PHOTOS = "photos"
  /** Progress and result of a send. */
  const val CHANNEL_SENDS = "sends"
  const val NOTIFY_PHOTO = 100
  const val NOTIFY_SEND = 101

  const val ACTION_SEND = "dev.relay.desktop.SEND_DROP"
  const val EXTRA_PEER = "peer"
  /** content:// URIs the service reads when it sends (new photos). */
  const val EXTRA_URIS = "uris"
  /** Files already copied into the cache (share sheet). */
  const val EXTRA_PATHS = "paths"

  /** Most "Send to" buttons a notification shows. */
  const val MAX_ACTIONS = 3
  private const val RESULT_SHOWN_MS = 6_000L

  fun createChannels(context: Context) {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
    val manager = context.getSystemService(NotificationManager::class.java)
    val photos = NotificationChannel(CHANNEL_PHOTOS, "New photos", NotificationManager.IMPORTANCE_HIGH)
    photos.description = "Offers to send each new photo to a computer"
    photos.setSound(null, null)
    photos.enableVibration(false)
    val sends = NotificationChannel(CHANNEL_SENDS, "Sending files", NotificationManager.IMPORTANCE_LOW)
    sends.setShowBadge(false)
    manager.createNotificationChannels(listOf(photos, sends))
  }

  /** Paired devices, from the engine's database. */
  fun peers(context: Context): List<String> =
    RelayNative.peers(RelaySyncService.home(context).absolutePath)
      .lines()
      .filter { it.isNotBlank() }

  /** Copy [uri] into cacheDir/drops under its display name. */
  fun copyToCache(context: Context, uri: Uri): File? {
    val name = displayName(context, uri) ?: "file"
    val dir = File(context.cacheDir, "drops/${System.nanoTime()}")
    return try {
      dir.mkdirs()
      val file = File(dir, name.replace('/', '_'))
      context.contentResolver.openInputStream(uri)?.use { input ->
        file.outputStream().use { input.copyTo(it) }
      } ?: return null
      file
    } catch (err: Exception) {
      Log.w(TAG, "copy $uri: $err")
      dir.deleteRecursively()
      null
    }
  }

  private fun displayName(context: Context, uri: Uri): String? =
    context.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)
      ?.use { cursor -> if (cursor.moveToFirst()) cursor.getString(0) else null }

  /** The intent that sends [uris] or [paths] to [peer] through the service. */
  fun sendIntent(context: Context, peer: String, uris: List<String> = emptyList(), paths: List<String> = emptyList()): Intent =
    Intent(context, RelaySyncService::class.java)
      .setAction(ACTION_SEND)
      .putExtra(EXTRA_PEER, peer)
      .putStringArrayListExtra(EXTRA_URIS, ArrayList(uris))
      .putStringArrayListExtra(EXTRA_PATHS, ArrayList(paths))

  /**
   * Run one send on a worker thread, showing progress and the result.
   * Called by [RelaySyncService] for [ACTION_SEND]. [onDone] runs on the main
   * thread just before the result is posted, so the service can leave the
   * foreground first and the result stays a normal, dismissible notification.
   */
  fun send(context: Context, intent: Intent, onDone: () -> Unit) {
    val peer = intent.getStringExtra(EXTRA_PEER)
    val uris = intent.getStringArrayListExtra(EXTRA_URIS).orEmpty()
    val paths = intent.getStringArrayListExtra(EXTRA_PATHS).orEmpty()
    if (peer == null || (uris.isEmpty() && paths.isEmpty())) {
      onDone()
      return
    }
    val manager = context.getSystemService(NotificationManager::class.java)
    val main = Handler(Looper.getMainLooper())
    manager.cancel(NOTIFY_PHOTO)
    PhotoWatch.clearPending(context)
    val count = uris.size + paths.size
    manager.notify(NOTIFY_SEND, progress(context, peer, count))

    Thread({
      val home = RelaySyncService.home(context).absolutePath
      var failure: String? = null
      val files = paths.map { File(it) } + uris.mapNotNull { copyToCache(context, Uri.parse(it)) }
      if (files.size < count) failure = "a file could not be read"
      for (file in files) {
        val error = RelayNative.dropSend(home, peer, file.absolutePath)
        file.parentFile?.deleteRecursively()
        if (error != null && failure == null) failure = error
      }
      val done = result(context, intent, peer, count, failure)
      main.post {
        onDone()
        manager.notify(NOTIFY_SEND, done)
      }
    }, "relay-drop").start()
  }

  /** The "Sending…" notification; also the service's foreground notification. */
  fun progress(context: Context, peer: String, count: Int) =
    NotificationCompat.Builder(context, CHANNEL_SENDS)
      .setSmallIcon(android.R.drawable.stat_sys_upload)
      .setContentTitle(if (count == 1) "Sending to $peer…" else "Sending $count files to $peer…")
      .setProgress(0, 0, true)
      .setOngoing(true)
      .setSilent(true)
      .build()

  private fun result(context: Context, intent: Intent, peer: String, count: Int, failure: String?): android.app.Notification {
    val builder = NotificationCompat.Builder(context, CHANNEL_SENDS)
      .setSilent(true)
      .setAutoCancel(true)
    if (failure == null) {
      return builder
        .setSmallIcon(android.R.drawable.stat_sys_upload_done)
        .setContentTitle(if (count == 1) "Sent to $peer" else "Sent $count files to $peer")
        .setTimeoutAfter(RESULT_SHOWN_MS)
        .build()
    }
    // Retry sends the same files again; copied ones are gone, so only
    // MediaStore URIs can be retried.
    val failed = builder
      .setSmallIcon(android.R.drawable.stat_notify_error)
      .setContentTitle("Couldn't send to $peer")
      .setContentText(failure)
      .setStyle(NotificationCompat.BigTextStyle().bigText(failure))
    val uris = intent.getStringArrayListExtra(EXTRA_URIS).orEmpty()
    if (uris.isNotEmpty() && Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
      val retry = PendingIntent.getForegroundService(
        context,
        peer.hashCode(),
        sendIntent(context, peer, uris = uris),
        PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
      )
      failed.addAction(0, "Retry", retry)
    }
    return failed.build()
  }
}
