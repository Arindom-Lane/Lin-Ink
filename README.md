# Pen Touch

Use a pen tablet (Wacom or any evdev tablet) like a **native touch screen** on Linux.

* One big **on/off switch** in a small window. It stays on/off across logouts and reboots until you change it.
* The pen becomes a real virtual multitouch screen, so every app handles it *natively*: touch scrolling with
  the app's own smooth inertia, long-press for the context menu, double-tap a word to select it (with the
  toolkit's own selection handles where it has them), tap to click.
* A cursor follows the pen all the time (also while you scroll). Optional.
* No modes. Want pressure for drawing? Flip the switch off: the tablet goes back to normal instantly.
* Single self-contained **AppImage**; works on any x86-64 distro with glibc 2.17+ (Ubuntu 18.04+, Debian 9+, Fedora, Arch, ...).

## Use it

1. Download `PenTouch-x86_64.AppImage`, `chmod +x` it, run it.
2. First run only: if it says **One-time setup needed**, press **Fix permissions** (asks for your password once).
   This installs a udev rule so *your user* may create the virtual touch screen and read the tablet. No restart.
3. Flip the switch. Done. Close the window if you like - it keeps running in the background and starts at login.

From a terminal: `PenTouch.AppImage on | off | toggle | status`.

| Gesture            | Result                                                     |
|--------------------|------------------------------------------------------------|
| tap                | click                                                      |
| drag               | scroll (the app's native touch scrolling, with inertia)    |
| hold               | right-click / context menu (the app's native long-press)   |
| double-tap on text | select a word; drag the handles to extend (where the app shows them) |

### Options (in the window)

* **Start automatically when I log in** (default on).
* **Show a cursor that follows the pen**. If some app hides its touch selection handles while the cursor moves,
  turn this off and you get a pure touch screen.
* **Tap tolerance** - how far the pen may wobble during a tap/long-press (default 2 mm). Hand tremor on a tablet
  would otherwise cancel taps and long-presses because 1 mm of pen is ~12 px on screen.
* **Tablet rotation** 0/90/180/270.
* **Cursor area** - for several monitors: which part of the desktop the cursor covers. For *touches*, pick the
  output in your desktop's own touch-screen settings (e.g. KDE: System Settings > Input Devices > Touchscreen).

## How it works

```
 pen tablet (grabbed) --> filter --> "Pen Touchscreen (virtual)"   multitouch screen, 1 finger
                            \-----> "Pen Cursor (virtual)"         absolute mouse, move only
```

The **filter** makes a pen behave like a finger: it ignores contact blips, holds the touch still inside the tap
tolerance (so taps and long-presses are steady), glides the touch to the pen instead of jumping when a drag
starts (a jump would look like a fling), bridges pressure flicker, and snaps a quick second tap onto the first so
double-tap is recognised.

The **background service** (`pen-touch --daemon`) owns the devices. It reads the saved switch
(`~/.config/pen-touch/config.toml`), starts at login (`~/.config/autostart/pen-touch.desktop`) and obeys the
window, which is only a remote control. Status is shared through `$XDG_RUNTIME_DIR/pen-touch/status.json`.

## Build

```sh
cargo install cargo-zigbuild --locked      # once
pip install ziglang                        # once
./build-appimage.sh                        # -> PenTouch-x86_64.AppImage
```

Plain `cargo build --release` also works (binary for your own glibc). `cargo test` runs 55 tests, including
end-to-end tests that run the real service and check the switch survives restarts.
`cargo build --no-default-features` builds without the window (service + CLI only).

AppImages need FUSE 2 to mount. If it is missing: `APPIMAGE_EXTRACT_AND_RUN=1 ./PenTouch-x86_64.AppImage`.

## Troubleshooting

* **"needs permission"** - press *Fix permissions*, or run `sudo ./PenTouch.AppImage --setup-permissions`.
  It writes `/etc/udev/rules.d/70-pen-touch.rules` (`uaccess` for `/dev/uinput` and tablets) and makes sure the
  `uinput` module loads at boot.
* **Nothing happens** - `PenTouch.AppImage status` shows what the service sees.
* **Cursor and touches land on different screens** - set the *Output* for "Pen Touchscreen (virtual)" in your
  system's touch settings, and the *Cursor area* in the window to the same monitor.
