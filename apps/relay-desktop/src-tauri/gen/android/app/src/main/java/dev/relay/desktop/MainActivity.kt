package dev.relay.desktop

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.view.View
import android.widget.Toast
import androidx.activity.enableEdgeToEdge
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat

/** Not in every compile SDK's Manifest.permission yet, so by name. */
private const val LOCAL_NETWORK = "android.permission.ACCESS_LOCAL_NETWORK"

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
    // Android 17 local network protection: without this, the phone cannot
    // find or reach computers on the same Wi-Fi.
    if (Build.VERSION.SDK_INT >= 37 &&
      checkSelfPermission(LOCAL_NETWORK) != PackageManager.PERMISSION_GRANTED
    ) {
      wanted.add(LOCAL_NETWORK)
    }
    // Offering each new photo or video to a computer reads the camera
    // folder (D50).
    if (PhotoWatch.supported()) {
      wanted.addAll(
        PhotoWatch.permissions().filter { checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED },
      )
    }
    if (wanted.isNotEmpty()) {
      requestPermissions(wanted.toTypedArray(), 0)
    }
    Drops.createChannels(this)
    PhotoWatch.schedule(this)
    // Sync runs in-process while the app is open; the service runs only
    // while a file is being sent, so nothing sits in the notifications.
    handlePairLink(intent)
  }

  // launchMode singleTask: a QR code scanned while Relay is open lands here.
  override fun onNewIntent(intent: Intent) {
    super.onNewIntent(intent)
    handlePairLink(intent)
  }

  /** Pair with the computer whose relay://pair QR code the camera opened. */
  private fun handlePairLink(intent: Intent?) {
    val link = intent?.data?.takeIf { it.scheme == "relay" && it.host == "pair" }?.toString() ?: return
    // Handle a link once, not again when the activity is recreated.
    intent.data = null
    Toast.makeText(this, "Pairing…", Toast.LENGTH_SHORT).show()
    val home = RelaySyncService.home(this).absolutePath
    Thread({
      val result = RelayNative.pairLink(home, link)
      runOnUiThread { Toast.makeText(this, result, Toast.LENGTH_LONG).show() }
    }, "relay-pair").start()
  }

  @Deprecated("Superseded by the activity result API; TauriActivity still routes here.")
  override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
    super.onRequestPermissionsResult(requestCode, permissions, grantResults)
    PhotoWatch.schedule(this)
  }
}
