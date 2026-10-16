package dev.relay.desktop

/** JNI entries in librelay_desktop_lib (src-tauri/src/mobile.rs). */
object RelayNative {
  init {
    System.loadLibrary("relay_desktop_lib")
  }

  /** Starts sync for [home] unless it is already running in this process. */
  external fun start(home: String)

  /** Stops sync and waits briefly for the engine to exit. */
  external fun stop()

  /** One status line for the notification, e.g. "Running, 2 peers online". */
  external fun status(): String

  /**
   * Starts sync if needed and sends the file at [path] to [peer] (D50).
   * Blocks for up to about 25 s while the computer connects; never call it on
   * the main thread. Returns null once it is saved there, else the reason.
   */
  external fun dropSend(home: String, peer: String, path: String): String?

  /** Paired devices, one name per line. */
  external fun peers(home: String): String
}
