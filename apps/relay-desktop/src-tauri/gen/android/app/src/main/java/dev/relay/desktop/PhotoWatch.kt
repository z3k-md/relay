package dev.relay.desktop

import android.Manifest
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.job.JobInfo
import android.app.job.JobParameters
import android.app.job.JobScheduler
import android.app.job.JobService
import android.content.BroadcastReceiver
import android.content.ComponentName
import android.content.ContentUris
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Bitmap
import android.net.Uri
import android.os.Build
import android.provider.MediaStore
import android.util.Log
import android.util.Size
import androidx.core.app.NotificationCompat


/**
 * Offers each new camera photo or video to a computer (photo-drop proposal,
 * stage 4).
 *
 * A JobScheduler job with content-URI triggers on MediaStore runs a few
 * seconds after the camera saves something, with nothing resident in
 * between. It posts one notification with a thumbnail and a "Send to …"
 * button per paired device. The file is read when the button is tapped, so
 * a Pixel's processed photo is the one that goes. Content-trigger jobs end
 * at reboot, so [DropReceiver] schedules the job again on boot.
 */
object PhotoWatch {
  private const val TAG = "RelayPhotos"
  private const val JOB_ID = 1001
  private const val PREFS = "photo_watch"
  private const val KEY_LAST_ID = "last_id"
  private const val KEY_PENDING = "pending_uris"
  private const val KEY_PENDING_AT = "pending_at"
  /** A notification nobody acted on is gone after this. */
  private const val OFFER_MS = 10 * 60 * 1000L
  /** Files one notification carries. */
  private const val MAX_PENDING = 20
  private const val CAMERA_FOLDER = "DCIM/Camera/"

  /** One MediaStore table to watch. IDs are shared across tables. */
  private class Table(val uri: Uri, val permission: String, val video: Boolean)

  private fun tables(): List<Table> =
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
      listOf(
        Table(MediaStore.Images.Media.EXTERNAL_CONTENT_URI, Manifest.permission.READ_MEDIA_IMAGES, false),
        Table(MediaStore.Video.Media.EXTERNAL_CONTENT_URI, Manifest.permission.READ_MEDIA_VIDEO, true),
      )
    } else {
      listOf(
        Table(MediaStore.Images.Media.EXTERNAL_CONTENT_URI, Manifest.permission.READ_EXTERNAL_STORAGE, false),
        Table(MediaStore.Video.Media.EXTERNAL_CONTENT_URI, Manifest.permission.READ_EXTERNAL_STORAGE, true),
      )
    }

  /** A finished camera file. */
  private class Item(val id: Long, val uri: String, val video: Boolean)

  /** Permissions to ask for; MainActivity requests the missing ones. */
  fun permissions(): List<String> = tables().map { it.permission }.distinct()

  /** Watching needs MediaStore's relative paths and pending flag (Android 10+). */
  fun supported(): Boolean = Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q

  private fun readable(context: Context): List<Table> =
    if (!supported()) {
      emptyList()
    } else {
      tables().filter { context.checkSelfPermission(it.permission) == PackageManager.PERMISSION_GRANTED }
    }

  fun allowed(context: Context): Boolean = readable(context).isNotEmpty()

  /** Arm the job for the next change in MediaStore. */
  fun schedule(context: Context) {
    val tables = readable(context)
    if (tables.isEmpty()) return
    // Start from now, so the first photo after install is offered rather
    // than taken as the starting point.
    val prefs = prefs(context)
    if (prefs.getLong(KEY_LAST_ID, -1L) < 0) {
      Thread({ prefs.edit().putLong(KEY_LAST_ID, latestId(context)).apply() }, "relay-photos-start").start()
    }
    val builder = JobInfo.Builder(JOB_ID, ComponentName(context, NewPhotoJob::class.java))
    for (table in tables) {
      builder.addTriggerContentUri(
        JobInfo.TriggerContentUri(table.uri, JobInfo.TriggerContentUri.FLAG_NOTIFY_FOR_DESCENDANTS),
      )
    }
    val job = builder
      .setTriggerContentUpdateDelay(500)
      .setTriggerContentMaxDelay(3_000)
      .build()
    context.getSystemService(JobScheduler::class.java).schedule(job)
  }

  fun clearPending(context: Context) {
    prefs(context).edit().remove(KEY_PENDING).remove(KEY_PENDING_AT).apply()
  }

  private fun prefs(context: Context) = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

  /** Find camera files added since the last look and offer them. */
  fun check(context: Context) {
    if (!allowed(context)) return
    val prefs = prefs(context)
    val last = prefs.getLong(KEY_LAST_ID, -1L)
    if (last < 0) {
      // First run: start from now instead of offering the whole camera roll.
      prefs.edit().putLong(KEY_LAST_ID, latestId(context)).apply()
      return
    }
    val fresh = newItems(context, last)
    if (fresh.isEmpty()) return
    prefs.edit().putLong(KEY_LAST_ID, fresh.maxOf { it.id }).apply()

    val manager = context.getSystemService(NotificationManager::class.java)
    if (!manager.areNotificationsEnabled()) return
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O &&
      manager.getNotificationChannel(Drops.CHANNEL_PHOTOS)?.importance == NotificationManager.IMPORTANCE_NONE
    ) {
      return
    }
    val peers = Drops.peers(context)
    if (peers.isEmpty()) return

    // Files still on offer from a notification shown a moment ago join
    // this one ("3 new photos").
    val now = System.currentTimeMillis()
    val earlier = if (now - prefs.getLong(KEY_PENDING_AT, 0L) < OFFER_MS) {
      prefs.getString(KEY_PENDING, "").orEmpty().lines().filter { it.isNotBlank() }
    } else {
      emptyList()
    }
    val pending = (earlier + fresh.map { it.uri }).distinct().takeLast(MAX_PENDING)
    prefs.edit()
      .putString(KEY_PENDING, pending.joinToString("\n"))
      .putLong(KEY_PENDING_AT, now)
      .apply()
    manager.notify(Drops.NOTIFY_PHOTO, offer(context, pending, fresh.last(), peers))
  }

  /**
   * Finished camera files after [after], oldest first. A file still being
   * written (a video while recording) comes back when it is finished, so
   * nothing at or after the first unfinished one is returned yet.
   */
  private fun newItems(context: Context, after: Long): List<Item> {
    val items = mutableListOf<Item>()
    var firstPending = Long.MAX_VALUE
    for (table in readable(context)) {
      try {
        context.contentResolver.query(
          table.uri,
          arrayOf(MediaStore.MediaColumns._ID, MediaStore.MediaColumns.IS_PENDING),
          "${MediaStore.MediaColumns._ID} > ? AND ${MediaStore.MediaColumns.RELATIVE_PATH} LIKE ?",
          arrayOf(after.toString(), "$CAMERA_FOLDER%"),
          "${MediaStore.MediaColumns._ID} ASC",
        )?.use { cursor ->
          while (cursor.moveToNext()) {
            val id = cursor.getLong(0)
            if (cursor.getInt(1) != 0) {
              firstPending = minOf(firstPending, id)
              break
            }
            items.add(Item(id, ContentUris.withAppendedId(table.uri, id).toString(), table.video))
          }
        }
      } catch (err: Exception) {
        Log.w(TAG, "query ${table.uri}: $err")
      }
    }
    return items.filter { it.id < firstPending }.sortedBy { it.id }
  }

  private fun latestId(context: Context): Long =
    readable(context).maxOfOrNull { table ->
      context.contentResolver.query(
        table.uri,
        arrayOf(MediaStore.MediaColumns._ID),
        null,
        null,
        "${MediaStore.MediaColumns._ID} DESC",
      )?.use { cursor -> if (cursor.moveToFirst()) cursor.getLong(0) else 0L } ?: 0L
    } ?: 0L

  private fun title(uris: List<String>): String {
    val videos = uris.count { it.contains("/video/") }
    val photos = uris.size - videos
    return when {
      uris.size == 1 && videos == 1 -> "New video"
      uris.size == 1 -> "New photo"
      videos == 0 -> "$photos new photos"
      photos == 0 -> "$videos new videos"
      else -> "${uris.size} new photos and videos"
    }
  }

  private fun offer(context: Context, uris: List<String>, newest: Item, peers: List<String>): android.app.Notification {
    val builder = NotificationCompat.Builder(context, Drops.CHANNEL_PHOTOS)
      .setSmallIcon(android.R.drawable.ic_menu_upload)
      .setContentTitle(title(uris))
      .setContentText("Send to ${peers.first()}?")
      .setCategory(NotificationCompat.CATEGORY_RECOMMENDATION)
      .setPriority(NotificationCompat.PRIORITY_HIGH)
      .setAutoCancel(true)
      .setTimeoutAfter(OFFER_MS)
      .setDeleteIntent(
        PendingIntent.getBroadcast(
          context,
          0,
          Intent(context, DropReceiver::class.java).setAction(DropReceiver.ACTION_DISMISSED),
          PendingIntent.FLAG_IMMUTABLE,
        ),
      )
    thumbnail(context, Uri.parse(newest.uri))?.let { bitmap ->
      builder.setLargeIcon(bitmap)
      builder.setStyle(
        NotificationCompat.BigPictureStyle().bigPicture(bitmap).bigLargeIcon(null as Bitmap?),
      )
    }
    for (peer in peers.take(Drops.MAX_ACTIONS)) {
      val send = PendingIntent.getForegroundService(
        context,
        peer.hashCode(),
        Drops.sendIntent(context, peer, uris = uris),
        PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
      )
      val label = if (uris.size == 1) "Send to $peer" else "Send all to $peer"
      builder.addAction(0, label, send)
    }
    return builder.build()
  }

  /** A frame for a video, a scaled copy for a photo. */
  private fun thumbnail(context: Context, uri: Uri): Bitmap? =
    try {
      context.contentResolver.loadThumbnail(uri, Size(768, 768), null)
    } catch (err: Exception) {
      Log.w(TAG, "thumbnail: $err")
      null
    }
}

/** Runs [PhotoWatch.check] after MediaStore changes, then re-arms itself. */
class NewPhotoJob : JobService() {
  override fun onStartJob(params: JobParameters): Boolean {
    Thread({
      try {
        PhotoWatch.check(this)
      } finally {
        PhotoWatch.schedule(this)
        jobFinished(params, false)
      }
    }, "relay-photos").start()
    return true
  }

  override fun onStopJob(params: JobParameters): Boolean = true
}

/** Re-arms the photo job after a reboot, and forgets a dismissed offer. */
class DropReceiver : BroadcastReceiver() {
  companion object {
    const val ACTION_DISMISSED = "dev.relay.desktop.PHOTO_OFFER_DISMISSED"
  }

  override fun onReceive(context: Context, intent: Intent) {
    when (intent.action) {
      Intent.ACTION_BOOT_COMPLETED, Intent.ACTION_MY_PACKAGE_REPLACED -> {
        Drops.createChannels(context)
        PhotoWatch.schedule(context)
      }
      ACTION_DISMISSED -> PhotoWatch.clearPending(context)
    }
  }
}
