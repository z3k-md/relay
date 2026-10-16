# Send a photo to a computer

**Status:** Stages 1 (drops, D50) and 2 (desktop popup) in review. The phone
stages need Android stage 2 (PR #19).

Take a photo on the phone, tap **Send to Desk** in the notification that
appears, and a few seconds later the photo pops up on the computer, ready
to copy, drag into another app, or open. Today the same trip goes through
Google Photos and a browser and takes a minute or more.

A drop is one file pushed to one paired device. It is not sync: nothing is
mirrored, no space is involved, and deleting either copy changes nothing on
the other. It rides the existing session, so it works wherever the two
devices already connect (LAN, Tailscale, hole punching, relay).

## Flow

1. The camera saves a photo. Relay notices within a few seconds.
2. A notification shows a thumbnail, **Send to Desk** (one action per
   computer, at most three), and Dismiss. A burst becomes one notification
   ("3 new photos"). It removes itself after 10 minutes.
3. The tap starts the host if it is not running, connects to that computer,
   and pushes the file. Progress and failure ("Couldn't reach Desk · Retry")
   show in the same notification.
4. The computer verifies the bytes, saves them to an inbox folder, and shows
   a popup.

Target: under 3 s from tap to popup on the same LAN. Each step logs a
timestamp (photo added, notification posted, tap, connected, bytes done,
popup shown) so the slow step is visible.

## Phone

### Noticing a photo

- **Content-URI job.** A WorkManager job with a content-URI trigger on
  `MediaStore.Images.Media.EXTERNAL_CONTENT_URI` (descendants), with an
  update delay of about 500 ms and a max delay of about 3 s. Nothing stays
  resident and there is no permanent notification. The job re-enqueues
  itself after each run. Content-trigger jobs do not survive a reboot, so a
  `BOOT_COMPLETED` receiver enqueues it again; scheduling a job from boot is
  allowed, unlike starting the `dataSync` service.
- **Observer while the service runs.** When `RelaySyncService` is up, a
  `ContentObserver` in it does the same with no job delay.
- **What counts.** Rows added since the last one seen, with `IS_PENDING = 0`,
  under `DCIM/Camera/`, and under `Pictures/Screenshots/` if the user turns
  screenshots on. A Pixel replaces the JPEG after HDR+ processing, so the
  file is read when the user taps, not when the notification is posted.
- **Permission.** `READ_MEDIA_IMAGES`. If the user grants only selected
  photos (Android 14 partial access), new photos are invisible; settings
  says so and offers the full grant.

The first thing to build in stage 4 is a timing check of the job trigger on
a real Pixel. If it is slow or unreliable while the phone is in use, the
fallback is the observer in a foreground service, with the permanent
notification that comes with it.

### Sending

- The notification action starts `RelaySyncService` with the drop as an
  extra. A tap on a notification exempts the app from the background
  start restriction on foreground services. If the host is already running,
  the drop goes straight to it.
- The engine cannot open `content://` URIs. Kotlin copies the image into
  `cacheDir/drops/` and passes the path through JNI or IPC
  (`drop_send(peer, path)`). The copy is deleted when the send finishes.
- **Share target.** "Send to Relay" (`ACTION_SEND` and `ACTION_SEND_MULTIPLE`
  for `image/*`, later any type) takes the same path. It needs no media
  permission and covers older photos and other apps.

## Computer

The wire, who may drop, the inbox, and the activity item are D50.
`relay drop send` makes computer-to-computer drops work too.

### Popup

- A small always-on-top window in the corner above the tray, showing the
  thumbnail, the sender, and Copy, Open, and Show in folder. Dragging the
  thumbnail drops the file into another app. It stays about 15 s, longer
  while hovered, and queues during a burst.
- **Copy** puts both an image (`CF_DIB`/PNG on Windows, an image on macOS)
  and the file itself (`CF_HDROP`, a file URL) on the clipboard, so it pastes
  into chat apps and file managers alike. This goes through a maintained
  safe wrapper (D43).
- A setting copies every arriving drop to the clipboard automatically.

## Stages

1. **Wire and CLI.** `FEATURE_DROP`, `relay drop send`, saving to the inbox,
   the activity item, and a loopback test with two hosts.
2. **Desktop popup.** The window, copy with both formats, drag-out, and the
   inbox setting.
3. **Android send.** The share target and the `drop_send` path from Kotlin.
   This proves sending before taking on the risk in detection.
4. **Android notice.** The trigger timing check, the content-URI job, the
   service observer, and the Send to notification.
5. **Polish.** Screenshots toggle, burst grouping, auto-copy, a Copy text
   action (Windows OCR, Vision on macOS), and stripping location on send.

## Play Store later

Sideloaded builds are the target (D33). If Relay ever goes on Play:

- Play limits `READ_MEDIA_IMAGES` to apps whose core purpose needs broad
  photo access, and the app must file a declaration. If that is refused,
  the fallback is the share target plus the system photo picker, with no
  notification after each photo.
- Play also needs a declared reason for each foreground service type. For
  the send, a user-initiated data transfer job (Android 14+) may fit better
  than starting the `dataSync` service.
- No `MANAGE_EXTERNAL_STORAGE`: drops never write shared storage on the
  phone.

## Open questions

- **Several computers.** One action per computer, or one default target with
  the rest under a long-press?
- **Reachability.** Should the phone skip the notification when no target
  is reachable? That needs a probe on each photo. Stage 4 always posts.
- **Video.** Same path under the cap, but `MediaStore.Video` needs its own
  trigger and `READ_MEDIA_VIDEO`.
- **The other direction.** The wire is symmetric. Receiving on the phone
  needs a MediaStore insert into `Pictures/Relay`.

## Out of scope

Sending without a tap (a later per-peer "send automatically on this Wi-Fi"
would build on stage 4), receiving on the phone, and iOS.
