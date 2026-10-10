# macOS 10.13 (High Sierra) build

The 10.13 binary uses CoreGraphics for capture and AudioQueue for audio.
It does not link ScreenCaptureKit or require the modern Swift toolchain.
Use the release build for interactive sessions; the debug build is much slower
when encoding bitmap updates.

```sh
MACOSX_DEPLOYMENT_TARGET=10.13 cargo build --release \
  --no-default-features --features legacy-capture
```

The default feature set still uses modern capture. The 10.13 build supports
`--enable-h264` with VideoToolbox, and rejects `--virtual-display`. CoreGraphics
capture requires an active display in the server user's GUI session. On the
tested 10.13.6 Mac, `--enable-h264 --fps 20 --bitrate 4` gave smoother video
than the bitmap path in Windows App. The legacy encoder explicitly completes
each submitted frame because this version of VideoToolbox otherwise retained
frames without delivering them. Clients that do not negotiate EGFX continue
to receive bitmap updates.

The client may request a different window size. The legacy capture backend
preserves the Mac display's aspect ratio and centers it with black bars when
needed. It compares 64-pixel tiles between screenshots and sends only changed
regions. Pass `--no-client-resolution` to keep the remote desktop at its native
resolution instead of adopting the client's window size.

## Separate desktop for a background user

High Sierra has no `CGVirtualDisplay` class or functions. Its built-in Remote
Management/Screen Sharing service can instead log a *different* user into a
separate graphical desktop while the console user keeps their own desktop:

1. Enable Remote Management or Screen Sharing and allow the RDP account.
2. From another Mac, connect with Screen Sharing as that account. When offered,
   select **Connect to a virtual display**, not the console user's screen.
3. Start the 10.13 `macrdp` binary as that account **in its Aqua GUI launchd
   domain**. A process started directly from SSH belongs to the Background
   domain; on the tested 10.13 host it captured B's display but mouse clicks
   were ignored. A per-user LaunchAgent loaded into `gui/$(id -u)` is the
   durable way to start it after login.
4. In B's graphical desktop, grant Accessibility permission to the exact
   `macrdp` executable and restart its LaunchAgent. Check the startup log for
   `Accessibility permission already granted`; without that line, the RDP
   pointer may move locally while clicks and keys do nothing. Rebuilding an
   unsigned executable can invalidate an earlier grant.
5. Connect the RDP client with the same account. Clipboard text, images, rich
   text, and file copy continue to use the existing CLIPRDR implementation.

On the tested 10.13.6 Mac, the console account and the Screen Sharing account
had separate Finder/Dock processes. From the latter account, CoreGraphics
listed a 1920×1080 display and `CGDisplayCreateImage` captured it. A plain SSH
login before the graphical login saw zero displays. Disconnecting Screen Sharing
without logging out left B's Finder, loginwindow, and virtual display alive; an
RDP client could then use the B desktop without an open VNC viewer. This does
not solve the first login after a reboot: the current server does not create B's
graphical session on the first RDP connection.

## Audio

High Sierra has no built-in system-audio capture API. The legacy RDPSND path
records an input through AudioQueue at 44.1 kHz stereo and can send PCM or AAC.
Route B's application audio to a loopback device, then select that device with
`--legacy-audio-device "OrayVirtualAudioDevice"` (or the name of another
installed loopback input). If omitted, the server uses the default input. The
server refuses a physical microphone as an audio source. Selecting the input by
name avoids changing A's default input. Audio from the two users cannot be
isolated if both are routed into the same loopback device.

The menu-bar GUI package targets macOS 13 and is not part of this build. Use
the command-line server and a user LaunchAgent on 10.13.

## Start automatically when B logs in graphically

Run these commands as B (`jomic` on the tested host), after building the legacy
release binary. The regular `dist/install.sh` builds the default modern backend,
so do not use it for 10.13.

```sh
mkdir -p "$HOME/.local/bin" "$HOME/Library/LaunchAgents" "$HOME/Library/Logs"
cp target/release/macrdp "$HOME/.local/bin/macrdp"
codesign -s - --force "$HOME/.local/bin/macrdp"
security add-generic-password -U -s macrdp -a "$(id -un)" -w
```

The last command prompts for B's Mac account password and stores it in B's
Keychain. Do not put that password in the plist or command arguments. Save
the following as `~/Library/LaunchAgents/com.user.macrdp.plist`, replacing
`/Users/jomic` with B's actual home directory:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>com.user.macrdp</string>
  <key>ProgramArguments</key><array>
    <string>/Users/jomic/.local/bin/macrdp</string>
    <string>--keychain</string>
    <string>--bind</string><string>0.0.0.0:3390</string>
    <string>--enable-drive-redirection</string>
    <string>--legacy-audio-device</string><string>OrayVirtualAudioDevice</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardOutPath</key><string>/Users/jomic/Library/Logs/macrdp.out.log</string>
  <key>StandardErrorPath</key><string>/Users/jomic/Library/Logs/macrdp.err.log</string>
</dict></plist>
```

To use the tested H.264 mode, add these entries to `ProgramArguments` before
loading the agent: `<string>--enable-h264</string>`,
`<string>--fps</string><string>20</string>`, and
`<string>--bitrate</string><string>4</string>`.

After stopping any manually started server on port 3390, load the agent into
B's **GUI** domain (not the SSH Background domain):

```sh
plutil -lint "$HOME/Library/LaunchAgents/com.user.macrdp.plist"
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.user.macrdp.plist"
launchctl print "gui/$(id -u)/com.user.macrdp" | head
```

The agent starts on each subsequent B graphical login. To restart it after a
binary replacement, run `launchctl kickstart -k "gui/$(id -u)/com.user.macrdp"`.
If Accessibility permission is not reported as granted, grant it to the
installed `~/.local/bin/macrdp` executable in B's graphical desktop and restart
the agent. The agent cannot create B's first graphical login after a reboot.
