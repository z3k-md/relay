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
import android.os.Build
import android.provider.MediaStore
import android.util.Log
import android.util.Size
import androidx.core.app.NotificationCompat

/**
 * Offers each new camera photo to a computer (photo-drop proposal, stage 4).
 *
 * A JobScheduler job with a content-URI trigger on MediaStore runs a few
 * seconds after the camera saves a photo, with nothing resident in between.
 * It posts one notification with a thumbnail and a "Send to …" button per
 * paired device. The photo is read when the button is tapped, so a Pixel's
 * processed version is the one that goes. Content-trigger jobs end at
 * reboot, so [DropReceiver] schedules the job again on boot.
 */
object PhotoWatch {
  private const val TAG = "RelayPhotos"
  private const val JOB_ID = 1001
  private const val PREFS = "photo_watch"
  private const val KEY_LAST_ID = "last_id"
  private const val KEY_PENDING = "pending"
  private const val KEY_PENDING_AT = "pending_at"
  /** A notification nobody acted on is gone after this. */
  private const val OFFER_MS = 10 * 60 * 1000L
  /** Photos one notification carries. */
  private const val MAX_PENDING = 20
  private const val CAMERA_FOLDER = "DCIM/Camera/"

  fun permission(): String =
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
      Manifest.permission.READ_MEDIA_IMAGES
    } else {
      Manifest.permission.READ_EXTERNAL_STORAGE
    }

  /** Watching needs MediaStore's relative paths and pending flag (Android 10+). */
  fun supported(): Boolean = Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q

  fun allowed(context: Context): Boolean =
    supported() && context.checkSelfPermission(permission()) == PackageManager.PERMISSION_GRANTED

  /** Arm the job for the next change in MediaStore. */
  fun schedule(context: Context) {
    if (!allowed(context)) return
    val job = JobInfo.Builder(JOB_ID, ComponentName(context, NewPhotoJob::class.java))
      .addTriggerContentUri(
        JobInfo.TriggerContentUri(
          MediaStore.Images.Media.EXTERNAL_CONTENT_URI,
          JobInfo.TriggerContentUri.FLAG_NOTIFY_FOR_DESCENDANTS,
        ),
      )
      .setTriggerContentUpdateDelay(500)
      .setTriggerContentMaxDelay(3_000)
      .build()
    context.getSystemService(JobScheduler::class.java).schedule(job)
  }

  fun clearPending(context: Context) {
    prefs(context).edit().remove(KEY_PENDING).remove(KEY_PENDING_AT).apply()
  }

  private fun prefs(context: Context) = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

  /** Find photos added since the last look and offer them. */
  fun check(context: Context) {
    if (!allowed(context)) return
    val prefs = prefs(context)
    val last = prefs.getLong(KEY_LAST_ID, -1L)
    val fresh = newPhotos(context, last)
    if (last < 0) {
      // First run: start from now instead of offering the whole camera roll.
      prefs.edit().putLong(KEY_LAST_ID, latestId(context)).apply()
      return
    }
    if (fresh.isEmpty()) return
    prefs.edit().putLong(KEY_LAST_ID, fresh.max()).apply()

    val manager = context.getSystemService(NotificationManager::class.java)
    if (!manager.areNotificationsEnabled()) return
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O &&
      manager.getNotificationChannel(Drops.CHANNEL_PHOTOS)?.importance == NotificationManager.IMPORTANCE_NONE
    ) {
      return
    }
    val peers = Drops.peers(context)
    if (peers.isEmpty()) return

    // Photos still on offer from a notification shown a moment ago join
    // this one ("3 new photos").
    val now = System.currentTimeMillis()
    val earlier = if (now - prefs.getLong(KEY_PENDING_AT, 0L) < OFFER_MS) {
      prefs.getString(KEY_PENDING, "").orEmpty().split(',').mapNotNull { it.toLongOrNull() }
    } else {
      emptyList()
    }
    val pending = (earlier + fresh).distinct().takeLast(MAX_PENDING)
    prefs.edit()
      .putString(KEY_PENDING, pending.joinToString(","))
      .putLong(KEY_PENDING_AT, now)
      .apply()
    manager.notify(Drops.NOTIFY_PHOTO, offer(context, pending, peers))
  }

  /** IDs of finished camera photos after [after], oldest first. */
  private fun newPhotos(context: Context, after: Long): List<Long> {
    val ids = mutableListOf<Long>()
    val projection = arrayOf(MediaStore.Images.Media._ID, MediaStore.Images.Media.IS_PENDING)
    val selection = "${MediaStore.Images.Media._ID} > ? AND ${MediaStore.Images.Media.RELATIVE_PATH} LIKE ?"
    val args = arrayOf(after.toString(), "$CAMERA_FOLDER%")
    try {
      context.contentResolver.query(
        MediaStore.Images.Media.EXTERNAL_CONTENT_URI,
        projection,
        selection,
        args,
        "${MediaStore.Images.Media._ID} ASC",
      )?.use { cursor ->
        while (cursor.moveToNext()) {
          // A photo still being written comes back when it is finished;
          // stop before it so it is not skipped.
          if (cursor.getInt(1) != 0) break
          ids.add(cursor.getLong(0))
        }
      }
    } catch (err: Exception) {
      Log.w(TAG, "query: $err")
    }
    return ids
  }

  private fun latestId(context: Context): Long =
    context.contentResolver.query(
      MediaStore.Images.Media.EXTERNAL_CONTENT_URI,
      arrayOf(MediaStore.Images.Media._ID),
      null,
      null,
      "${MediaStore.Images.Media._ID} DESC",
    )?.use { cursor -> if (cursor.moveToFirst()) cursor.getLong(0) else 0L } ?: 0L

  private fun offer(context: Context, ids: List<Long>, peers: List<String>): android.app.Notification {
    val uris = ids.map {
      ContentUris.withAppendedId(MediaStore.Images.Media.EXTERNAL_CONTENT_URI, it).toString()
    }
    val title = if (ids.size == 1) "New photo" else "${ids.size} new photos"
    val builder = NotificationCompat.Builder(context, Drops.CHANNEL_PHOTOS)
      .setSmallIcon(android.R.drawable.ic_menu_upload)
      .setContentTitle(title)
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
    thumbnail(context, ids.last())?.let { bitmap ->
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
      val label = if (ids.size == 1) "Send to $peer" else "Send all to $peer"
      builder.addAction(0, label, send)
    }
    return builder.build()
  }

  private fun thumbnail(context: Context, id: Long): Bitmap? =
    try {
      val uri = ContentUris.withAppendedId(MediaStore.Images.Media.EXTERNAL_CONTENT_URI, id)
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
