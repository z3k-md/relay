package dev.relay.desktop

import android.app.Activity
import android.app.AlertDialog
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.widget.Toast
import androidx.core.content.ContextCompat

/**
 * "Send to Relay" in the share sheet (D50). Copies the shared files while
 * this activity still holds the grant to read them, then hands the copies to
 * [RelaySyncService]. With more than one paired device it asks which one.
 */
class SendToRelayActivity : Activity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    super.onCreate(savedInstanceState)
    val uris = sharedUris(intent)
    if (uris.isEmpty()) {
      finish()
      return
    }
    val peers = Drops.peers(this)
    when (peers.size) {
      0 -> {
        Toast.makeText(this, "Pair Relay with a computer first", Toast.LENGTH_LONG).show()
        finish()
      }
      1 -> send(peers[0], uris)
      else -> AlertDialog.Builder(this)
        .setTitle("Send to")
        .setItems(peers.toTypedArray()) { _, which -> send(peers[which], uris) }
        .setOnCancelListener { finish() }
        .show()
    }
  }

  private fun send(peer: String, uris: List<Uri>) {
    Toast.makeText(this, "Sending to $peer", Toast.LENGTH_SHORT).show()
    Thread({
      val paths = uris.mapNotNull { Drops.copyToCache(this, it)?.absolutePath }
      runOnUiThread {
        if (paths.isEmpty()) {
          Toast.makeText(this, "Relay couldn't read that file", Toast.LENGTH_LONG).show()
        } else {
          ContextCompat.startForegroundService(this, Drops.sendIntent(this, peer, paths = paths))
        }
        finish()
      }
    }, "relay-share").start()
  }

  @Suppress("DEPRECATION")
  private fun sharedUris(intent: Intent): List<Uri> =
    when (intent.action) {
      Intent.ACTION_SEND -> listOfNotNull(intent.getParcelableExtra<Uri>(Intent.EXTRA_STREAM))
      Intent.ACTION_SEND_MULTIPLE -> intent.getParcelableArrayListExtra<Uri>(Intent.EXTRA_STREAM).orEmpty()
      else -> emptyList()
    }
}
