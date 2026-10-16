package dev.relay.desktop

import android.Manifest
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import androidx.activity.enableEdgeToEdge

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    val wanted = mutableListOf<String>()
    // The service shows sync status; without this permission (Android 13+)
    // it still runs, but its notification is hidden.
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
      checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
    ) {
      wanted.add(Manifest.permission.POST_NOTIFICATIONS)
    }
    // Offering each new photo to a computer reads the camera folder (D50).
    if (PhotoWatch.supported() && !PhotoWatch.allowed(this)) {
      wanted.add(PhotoWatch.permission())
    }
    if (wanted.isNotEmpty()) {
      requestPermissions(wanted.toTypedArray(), 0)
    }
    Drops.createChannels(this)
    PhotoWatch.schedule(this)
    RelaySyncService.start(this)
  }

  @Deprecated("Superseded by the activity result API; TauriActivity still routes here.")
  override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
    super.onRequestPermissionsResult(requestCode, permissions, grantResults)
    PhotoWatch.schedule(this)
  }
}
