package dev.relay.desktop

import android.Manifest
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.view.View
import androidx.activity.enableEdgeToEdge
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    // Edge to edge draws the page under the status and navigation bars, and
    // the WebView does not report those insets to CSS. Pad the content so
    // the header sits below the status bar.
    val content = findViewById<View>(android.R.id.content)
    ViewCompat.setOnApplyWindowInsetsListener(content) { view, insets ->
      val bars = insets.getInsets(
        WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout() or WindowInsetsCompat.Type.ime(),
      )
      view.setPadding(bars.left, bars.top, bars.right, bars.bottom)
      WindowInsetsCompat.CONSUMED
    }
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
