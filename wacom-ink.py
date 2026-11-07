#!/usr/bin/env python3
"""
wacom_touch.py - make a Wacom (or any evdev) pen tablet behave like a touch screen,
similar to Windows Ink "pen as touch".

    sudo python3 wacom_touch.py                  auto-detect the pen
    sudo python3 wacom_touch.py --list           show detected pen devices
    sudo python3 wacom_touch.py --device /dev/input/event7 --brush --debug
    sudo python3 wacom_touch.py --device "Intuos"        # match by name substring
    sudo python3 wacom_touch.py --speed 120 --no-kinetic --no-tap-drag

TOUCH MODE (default)
    quick tap ................ left click  (tap-tap = double click, tap-tap-tap = triple)
    drag straight away ....... touch-scroll (up/down/left/right) with momentum after a flick
    hold 0.35 s, then drag .. select text / drag-and-drop
    tap, then drag ........... select / drag-and-drop starting at the tap (Windows Ink style)
    hold still 0.55 s ....... right click
    side button 1 (held) ..... that stroke is passed through like a normal pen (select / paint)
    side button 2 (click) .... toggle BRUSH MODE

BRUSH MODE
    The pen is passed through untouched: real pressure, tilt, hover distance, eraser end,
    proximity in/out. Use it for Photoshop / Krita / etc. Side button 1 is forwarded to the
    app; side button 2 stays the mode toggle. The eraser end always works as an eraser.

NOTES
    * The real pen is grabbed. The desktop sees two virtual devices:
        "Virtual Wacom Ink Pen"     (cursor, clicks, painting)
        "Virtual Wacom Ink Scroll"  (wheel + right click)
      The pen copies the real tablet's USB id, so existing GNOME/KDE tablet settings
      (map to monitor, keep proportions, ...) normally apply to it too.
    * The tablet is remembered by name + USB id, so unplug / re-plug keeps working even
      when the kernel assigns it a new /dev/input/eventN number.
    * Needs root (or access to /dev/input/event* and /dev/uinput).  If uinput is missing:
        sudo modprobe uinput
    * Needs python-evdev:  sudo apt install python3-evdev   (or: sudo pip install evdev)
"""
import argparse
import errno
import math
import os
import select
import signal
import sys
import time
from collections import deque

try:
    import evdev
    from evdev import InputDevice, UInput, ecodes as e
    from evdev.uinput import UInputError
except ImportError:
    sys.exit("Error: the python-evdev module is missing.\n"
             "Install it first, e.g.:\n"
             "  Debian/Ubuntu : sudo apt install python3-evdev\n"
             "  Fedora        : sudo dnf install python3-evdev\n"
             "  generic       : sudo pip install evdev")

# ============================ TUNABLE SETTINGS ============================
# --- scrolling
SCROLL_SPEED      = 95.0    # wheel notches for a drag across the full tablet height (higher = faster)
NATURAL_SCROLL    = True    # True = content follows the pen like a phone
AXIS_LOCK         = True    # lock a drag to vertical OR horizontal so a wobbly stroke can't scroll sideways
AXIS_LOCK_RATIO   = 1.8     #   how clearly one axis must dominate at the start of a drag to lock to it
AXIS_BREAKOUT_MM  = 12.0    #   moving this far off-axis unlocks free (diagonal) scrolling
KINETIC_SCROLL    = True    # keep gliding after a quick flick, like a phone
FLICK_MIN_SPEED   = 110.0   #   mm/s the pen must still have when lifted to count as a flick
FLICK_MAX_SPEED   = 1200.0  #   mm/s cap for the glide's starting speed
KINETIC_FRICTION  = 3.2     #   higher = glide stops sooner

# --- gestures
MOVE_SLOP_MM      = 1.5     # pen wobble tolerated before a touch turns into a drag
HOLD_SELECT_TIME  = 0.35    # hold still this long, then drag -> select / drag-and-drop
LONG_PRESS_RCLICK = 0.55    # hold still this long -> right click
MIN_TAP_TIME      = 0.025   # contacts shorter than this are electrical noise, not taps
LIFT_DEBOUNCE     = 0.03    # a lift shorter than this is pressure flicker and is ignored
DOUBLE_TAP_TIME   = 0.45    # a 2nd tap within this time ...
DOUBLE_TAP_MM     = 3.0     #   ... and this close is snapped onto the 1st one so double-click registers
TAP_DRAG_SELECT   = True    # tap, then touch near it and drag -> select-drag (like Windows Ink)
TAP_DRAG_MM       = 4.0     #   how far the 2nd touch must drag before it counts as select
CLICK_HOLD        = 0.025   # how long a synthetic click stays pressed
CLICK_PRESSURE    = 0.35    # fraction of max pressure used for synthetic clicks / selection drags

# --- brush mode
START_IN_BRUSH_MODE = False
BRUSH_PASS_BUTTON1  = True  # in brush mode, side button 1 is forwarded to the application

# --- virtual devices
VIRTUAL_PREFIX     = "Virtual Wacom Ink"
VIRTUAL_PEN_NAME   = VIRTUAL_PREFIX + " Pen"
VIRTUAL_MOUSE_NAME = VIRTUAL_PREFIX + " Scroll"
CLONE_USB_ID       = True   # give the virtual pen the real tablet's vendor/product id
# ==========================================================================

# internals
VELOCITY_WINDOW   = 0.10        # seconds of pen history used to measure flick speed
STILL_BEFORE_LIFT = 0.06        # a pen that rested this long before lifting is not a flick
COAST_STEP        = 1.0 / 120   # momentum update interval
COAST_STOP_SPEED  = 40.0        # wheel units/s below which momentum ends (120 units = 1 notch)

TOOL_BUTTONS = (e.BTN_TOOL_PEN, e.BTN_TOOL_RUBBER, e.BTN_TOOL_BRUSH,
                e.BTN_TOOL_PENCIL, e.BTN_TOOL_AIRBRUSH)

IDLE, PENDING, SCROLL, DRAG, PASS, RCLICK = "IDLE", "PENDING", "SCROLL", "DRAG", "PASS", "RCLICK"


# ------------------------------------------------------------------ devices
class PenState:
    """Last values reported by the physical pen."""

    def __init__(self):
        self.x = self.y = self.pressure = self.distance = 0
        self.tilt_x = self.tilt_y = self.misc = self.serial = self.scan = 0
        self.touch = self.btn1 = self.btn2 = False
        self.tool = None        # active BTN_TOOL_* code; None = pen is out of proximity
        self.extra = {}         # any other absolute axes (Art Pen rotation, airbrush wheel, ...)


class VirtualPen:
    """The uinput tablet the desktop sees. Only values that actually changed are written."""

    def __init__(self, src):
        caps = {}
        for etype, codes in src.capabilities().items():
            if etype in (e.EV_SYN, e.EV_FF):
                continue
            if etype == e.EV_ABS:   # fuzz=0: the real pen already filtered its own jitter
                codes = [(c, info._replace(fuzz=0)) for c, info in codes]
            caps[etype] = list(codes)
        self.abs = {c for c, _ in caps.get(e.EV_ABS, [])}
        self.keys = set(caps.get(e.EV_KEY, []))
        self.msc = set(caps.get(e.EV_MSC, []))

        ids = {}
        if CLONE_USB_ID:
            i = src.info
            ids = dict(vendor=i.vendor, product=i.product, version=i.version, bustype=i.bustype)
        props = list(src.input_props()) if hasattr(src, "input_props") else []
        try:
            self.ui = UInput(caps, name=VIRTUAL_PEN_NAME, input_props=props, **ids)
        except TypeError:           # very old python-evdev without input_props
            self.ui = UInput(caps, name=VIRTUAL_PEN_NAME, **ids)
        self.cur = {}
        self.tool = None
        self.dirty = False

    def _put(self, etype, code, value, force=False):
        key = (etype, code)
        if not force and self.cur.get(key) == value:
            return
        self.cur[key] = value
        self.ui.write(etype, code, value)
        self.dirty = True

    def _abs(self, code, value):
        if code in self.abs:
            self._put(e.EV_ABS, code, int(value))

    def _key(self, code, value):
        if code in self.keys:
            self._put(e.EV_KEY, code, 1 if value else 0)

    def send(self, s, tool, x, y, pressure, contact, stylus=False):
        """Write one complete pen frame.
        s supplies tilt/distance/etc; the caller chooses tool (None = out of proximity),
        position, pressure and whether the tip is down."""
        if tool is None:                                   # pen left the tablet
            self._key(e.BTN_TOUCH, 0)
            self._key(e.BTN_STYLUS, 0)
            self._abs(e.ABS_PRESSURE, 0)
            for t in TOOL_BUTTONS:
                self._key(t, 0)
            self.tool = None
        else:
            self._abs(e.ABS_X, x)
            self._abs(e.ABS_Y, y)
            self._abs(e.ABS_PRESSURE, pressure)
            self._abs(e.ABS_DISTANCE, 0 if contact else s.distance)
            self._abs(e.ABS_TILT_X, s.tilt_x)
            self._abs(e.ABS_TILT_Y, s.tilt_y)
            for code, value in s.extra.items():
                self._abs(code, value)
            if self.tool != tool:                          # entering proximity / pen <-> eraser
                if e.MSC_SERIAL in self.msc:
                    self._put(e.EV_MSC, e.MSC_SERIAL, s.serial, force=True)
                if e.MSC_SCAN in self.msc:                 # libinput uses this for tool detection
                    self._put(e.EV_MSC, e.MSC_SCAN, s.scan, force=True)
                if e.ABS_MISC in self.abs:
                    self._put(e.EV_ABS, e.ABS_MISC, s.misc, force=True)
                for t in TOOL_BUTTONS:
                    self._key(t, t == tool)
                self.tool = tool
            self._key(e.BTN_STYLUS, stylus)
            self._key(e.BTN_TOUCH, contact)
        if self.dirty:
            self.ui.syn()
            self.dirty = False

    def close(self):
        self.ui.close()


class VirtualMouse:
    """Virtual wheel + buttons. Fractional wheel movement is carried over, never lost."""

    def __init__(self):
        caps = {
            e.EV_KEY: [e.BTN_LEFT, e.BTN_RIGHT, e.BTN_MIDDLE],
            e.EV_REL: [e.REL_X, e.REL_Y, e.REL_WHEEL, e.REL_HWHEEL,
                       e.REL_WHEEL_HI_RES, e.REL_HWHEEL_HI_RES],
        }
        self.ui = UInput(caps, name=VIRTUAL_MOUSE_NAME)
        self.reset()

    def reset(self):
        self.frac = [0.0, 0.0]      # sub-unit remainder          [horizontal, vertical]
        self.notch = [0, 0]         # hi-res units not yet turned into a whole notch

    def scroll(self, hx, hy):
        """hx / hy in high-resolution wheel units (120 = one notch)."""
        wrote = False
        axes = ((hx, e.REL_HWHEEL_HI_RES, e.REL_HWHEEL), (hy, e.REL_WHEEL_HI_RES, e.REL_WHEEL))
        for i, (amount, hi_code, lo_code) in enumerate(axes):
            self.frac[i] += amount
            n = int(self.frac[i])               # truncates toward zero: symmetric for +/-
            if not n:
                continue
            self.frac[i] -= n
            self.ui.write(e.EV_REL, hi_code, n)
            self.notch[i] += n
            ticks = int(self.notch[i] / 120)
            if ticks:
                self.notch[i] -= ticks * 120
                self.ui.write(e.EV_REL, lo_code, ticks)
            wrote = True
        if wrote:
            self.ui.syn()

    def button(self, code, down):
        self.ui.write(e.EV_KEY, code, 1 if down else 0)
        self.ui.syn()

    def close(self):
        self.ui.close()


# ------------------------------------------------------------------ gesture engine
class TouchEmulator:
    def __init__(self, pen, vpen, vmouse, brush_mode=False, clock=time.monotonic, debug=False):
        self.pen, self.vpen, self.vmouse = pen, vpen, vmouse
        self.brush_mode, self.clock, self.debug = brush_mode, clock, debug

        self.axes = {c for c, _ in pen.capabilities().get(e.EV_ABS, [])}
        ax, ay = pen.absinfo(e.ABS_X), pen.absinfo(e.ABS_Y)
        xr, yr = max(1, ax.max - ax.min), max(1, ay.max - ay.min)
        self.upm_x = ax.resolution or xr / 150.0    # tablet units per mm
        self.upm_y = ay.resolution or yr / 100.0
        self.kx = 120.0 * SCROLL_SPEED / xr         # wheel units (120 = 1 notch) per tablet unit
        self.ky = 120.0 * SCROLL_SPEED / yr
        self.dir = 1 if NATURAL_SCROLL else -1
        pmax = pen.absinfo(e.ABS_PRESSURE).max if e.ABS_PRESSURE in self.axes else 0
        self.p_click = max(1, int(pmax * CLICK_PRESSURE))
        self.size_mm = (xr / self.upm_x, yr / self.upm_y)

        self.s = PenState()
        self.timers = {}                # name -> deadline (self.clock() time)
        self.mode = IDLE
        self.gtool = None               # tool that started the current gesture
        self.t0 = 0.0                   # when the current touch began
        self.sx = self.sy = 0           # where it began
        self.lx = self.ly = 0           # last position used for scrolling
        self.lock = None                # 'v' / 'h' axis lock while scrolling
        self.samples = deque(maxlen=64)  # (time, dx, dy) scroll history -> flick speed
        self.lift_pending = False
        self.lift_time = 0.0
        self.click_lock = False         # a synthetic click is in flight
        self.click_pos = (0, 0)
        self.last_tap = None            # (time, x, y)
        self.swallow_tap = False        # this touch only stopped a glide: don't click
        self.coasting = False
        self.coast = [0.0, 0.0]
        self.coast_t = 0.0
        self.dropped = False
        self._resync()

    # ---- helpers
    def _mm(self, dx, dy):
        return math.hypot(dx / self.upm_x, dy / self.upm_y)

    def _mode(self, new):
        if self.debug and new != self.mode:
            print(f"  [{self.clock() % 1000:8.3f}] {self.mode:8s} -> {new}")
        self.mode = new

    def _passthrough(self):
        return self.brush_mode or self.s.tool == e.BTN_TOOL_RUBBER

    def _fwd_btn1(self):
        return self.brush_mode and BRUSH_PASS_BUTTON1 and self.s.btn1

    # ---- virtual pen output
    def _hover(self):       # follow the pen in the air; tip up, pressure forced to 0
        s = self.s
        self.vpen.send(s, s.tool, s.x, s.y, 0, False, self._fwd_btn1())

    def _mirror(self):      # 1:1 copy of the physical pen (brush mode / eraser)
        s = self.s
        self.vpen.send(s, s.tool, s.x, s.y, s.pressure, s.touch, self._fwd_btn1())

    def _hold(self, x, y):  # park the virtual pen hovering at a fixed point
        self.vpen.send(self.s, self.gtool, x, y, 0, False)

    def _contact(self, x, y):   # synthetic "tip down" at x, y
        self.vpen.send(self.s, self.gtool, x, y, self.p_click, True)

    # ---- timers
    def next_deadline(self):
        return min(self.timers.values()) if self.timers else None

    def tick(self):
        now = self.clock()
        while True:
            due = [(t, n) for n, t in self.timers.items() if t <= now]
            if not due:
                return
            _, name = min(due)
            del self.timers[name]
            getattr(self, "_timer_" + name)(now)

    # ---- raw event intake
    def feed(self, ev):
        t, c, v = ev.type, ev.code, ev.value
        s = self.s
        if t == e.EV_SYN:
            if c == e.SYN_DROPPED:          # kernel buffer overflowed: ignore until next report
                self.dropped = True
            elif c == e.SYN_REPORT:
                if self.dropped:
                    self.dropped = False
                    self._resync()
                self._frame()
            return
        if self.dropped:
            return
        if t == e.EV_ABS:
            if c == e.ABS_X:
                s.x = v
            elif c == e.ABS_Y:
                s.y = v
            elif c == e.ABS_PRESSURE:
                s.pressure = v
            elif c == e.ABS_DISTANCE:
                s.distance = v
            elif c == e.ABS_TILT_X:
                s.tilt_x = v
            elif c == e.ABS_TILT_Y:
                s.tilt_y = v
            elif c == e.ABS_MISC:
                s.misc = v
            elif c in self.axes:
                s.extra[c] = v
        elif t == e.EV_KEY:
            if c == e.BTN_TOUCH:
                s.touch = bool(v)
            elif c in TOOL_BUTTONS:
                if v:
                    s.tool = c
                elif s.tool == c:
                    s.tool = None
            elif c == e.BTN_STYLUS:
                s.btn1 = bool(v)
            elif c == e.BTN_STYLUS2:
                down = bool(v)
                if down and not s.btn2 and self.mode == IDLE and not s.touch:
                    self.brush_mode = not self.brush_mode
                    self._stop_coasting(sync=False)   # don't leave a glide running in the new mode
                    print("\n>>> Switched to: " + (
                        "BRUSH MODE (full pen passthrough: pressure, tilt, eraser)"
                        if self.brush_mode else "TOUCH MODE (tap / scroll / select)"))
                s.btn2 = down
        elif t == e.EV_MSC:
            if c == e.MSC_SERIAL:
                s.serial = v
            elif c == e.MSC_SCAN:
                s.scan = v

    def _resync(self):
        """Re-read the pen state from the kernel (at start-up and after SYN_DROPPED)."""
        s, pen = self.s, self.pen
        keys = set(pen.active_keys())
        s.touch = e.BTN_TOUCH in keys
        s.btn1 = e.BTN_STYLUS in keys
        s.btn2 = e.BTN_STYLUS2 in keys
        s.tool = next((t for t in TOOL_BUTTONS if t in keys), None)
        for attr, code in (("x", e.ABS_X), ("y", e.ABS_Y), ("pressure", e.ABS_PRESSURE),
                           ("distance", e.ABS_DISTANCE), ("tilt_x", e.ABS_TILT_X),
                           ("tilt_y", e.ABS_TILT_Y), ("misc", e.ABS_MISC)):
            if code in self.axes:
                setattr(s, attr, pen.absinfo(code).value)
        for code in self.axes:               # extra axes: Art Pen rotation, airbrush wheel, ...
            if code not in (e.ABS_X, e.ABS_Y, e.ABS_PRESSURE, e.ABS_DISTANCE,
                            e.ABS_TILT_X, e.ABS_TILT_Y, e.ABS_MISC):
                s.extra[code] = pen.absinfo(code).value

    # ---- one report from the pen
    def _frame(self):
        now = self.clock()
        s = self.s
        if self.click_lock:                 # a synthetic click is in flight: keep the virtual pen still
            return
        m = self.mode
        if m == IDLE:
            if s.touch and s.tool is not None:
                self._begin_touch(now)
            elif not self.coasting:         # (cursor stays parked while a glide is running)
                if self._passthrough():
                    self._mirror()
                else:
                    self._hover()
            return
        if m == PASS:
            self._mirror()
            if not s.touch:
                self._mode(IDLE)
            return
        # gesture modes PENDING / SCROLL / DRAG / RCLICK, with lift debouncing
        if not s.touch:
            if not self.lift_pending:
                self.lift_pending = True
                self.lift_time = now
                self.timers["lift"] = now + LIFT_DEBOUNCE
            return
        if self.lift_pending:               # contact came back: it was only pressure flicker
            self.lift_pending = False
            self.timers.pop("lift", None)
        if m == PENDING:
            self._do_pending(now)
        elif m == SCROLL:
            self._do_scroll(now)
        elif m == DRAG:
            if s.btn1:                      # side button pressed mid-drag: become a real stroke
                self._mode(PASS)
                self._mirror()
            else:
                self._contact(s.x, s.y)

    def _begin_touch(self, now):
        s = self.s
        self.t0 = now
        self.sx, self.sy = s.x, s.y
        self.gtool = s.tool
        self.swallow_tap = self.coasting
        self._stop_coasting(sync=False)
        if s.tool == e.BTN_TOOL_RUBBER or self.brush_mode or s.btn1:
            self._mode(PASS)
            self._mirror()
            return
        self._mode(PENDING)
        self._hold(self.sx, self.sy)        # cursor lands here, tip stays UP so nothing selects
        self.timers["longpress"] = now + LONG_PRESS_RCLICK

    def _do_pending(self, now):
        s = self.s
        if s.btn1:                          # side button pressed after touching down
            self.timers.pop("longpress", None)
            self._mode(PASS)
            self._mirror()
            return
        moved = self._mm(s.x - self.sx, s.y - self.sy)
        if moved < MOVE_SLOP_MM:
            return
        lt = self.last_tap
        if (TAP_DRAG_SELECT and lt is not None
                and now - lt[0] <= DOUBLE_TAP_TIME
                and self._mm(self.sx - lt[1], self.sy - lt[2]) <= DOUBLE_TAP_MM):
            if moved < TAP_DRAG_MM:         # small drift after a tap: keep waiting, stay a tap
                return
            self.timers.pop("longpress", None)
            self._mode(DRAG)
            self.last_tap = None
            self._contact(lt[1], lt[2])     # press where the tap landed ...
            self._contact(s.x, s.y)         # ... and drag to here
            return
        self.timers.pop("longpress", None)
        if now - self.t0 >= HOLD_SELECT_TIME:   # held, then moved -> press-and-drag
            self._mode(DRAG)
            self._contact(self.sx, self.sy)     # press where the hold began ...
            self._contact(s.x, s.y)             # ... and drag to here
        else:                                   # moved right away -> touch-scroll
            self._mode(SCROLL)
            self.lx, self.ly = s.x, s.y
            self.samples.clear()
            self.vmouse.reset()
            self.lock = None
            if AXIS_LOCK:
                ddx = abs(s.x - self.sx) / self.upm_x
                ddy = abs(s.y - self.sy) / self.upm_y
                if ddy >= ddx * AXIS_LOCK_RATIO:
                    self.lock = "v"
                elif ddx >= ddy * AXIS_LOCK_RATIO:
                    self.lock = "h"

    def _do_scroll(self, now):
        s = self.s
        if s.btn1:                          # side button pressed mid-scroll: become a real stroke
            self._mode(PASS)
            self._mirror()
            return
        dx, dy = s.x - self.lx, s.y - self.ly
        self.lx, self.ly = s.x, s.y
        if self.lock == "v":
            if abs(s.x - self.sx) / self.upm_x >= AXIS_BREAKOUT_MM:
                self.lock = None
            else:
                dx = 0
        elif self.lock == "h":
            if abs(s.y - self.sy) / self.upm_y >= AXIS_BREAKOUT_MM:
                self.lock = None
            else:
                dy = 0
        if dx or dy:
            self.samples.append((now, dx, dy))
            self.vmouse.scroll(-dx * self.kx * self.dir, dy * self.ky * self.dir)

    # ---- timer callbacks
    def _timer_longpress(self, now):
        if self.mode != PENDING:
            return
        if self.lift_pending:               # might just be flicker - look again shortly
            self.timers["longpress"] = now + LIFT_DEBOUNCE
            return
        self._mode(RCLICK)
        self.vmouse.button(e.BTN_RIGHT, True)
        self.timers["rclick_up"] = now + CLICK_HOLD

    def _timer_rclick_up(self, now):
        self.vmouse.button(e.BTN_RIGHT, False)

    def _timer_lift(self, now):
        """The pen has really left the surface: finish whatever gesture was running."""
        if not self.lift_pending:
            return
        self.lift_pending = False
        mode = self.mode
        self.timers.pop("longpress", None)
        self._mode(IDLE)
        if mode == PENDING:
            if self.lift_time - self.t0 >= MIN_TAP_TIME and not self.swallow_tap:
                self._tap(now)
            else:
                self._hover()
        elif mode == SCROLL:
            if not self._start_coasting(now):
                self._hover()
        else:                               # DRAG: release the tip; RCLICK: nothing was pressed
            self._hover()

    def _tap(self, now):
        x, y = self.sx, self.sy
        lt = self.last_tap
        if lt and now - lt[0] <= DOUBLE_TAP_TIME and self._mm(x - lt[1], y - lt[2]) <= DOUBLE_TAP_MM:
            x, y = lt[1], lt[2]             # snap onto the previous tap so double-click registers
        self.last_tap = (now, x, y)
        self.click_pos = (x, y)
        self.click_lock = True
        self._contact(x, y)
        self.timers["click_up"] = now + CLICK_HOLD

    def _timer_click_up(self, now):
        x, y = self.click_pos
        self.vpen.send(self.s, self.gtool, x, y, 0, False)     # release exactly where we pressed
        self.click_lock = False
        self._frame()                       # then follow the real pen again

    # ---- momentum scrolling
    def _start_coasting(self, now):
        if not KINETIC_SCROLL:
            return False
        pts = [p for p in self.samples if self.lift_time - p[0] <= VELOCITY_WINDOW]
        if len(pts) < 2 or self.lift_time - pts[-1][0] > STILL_BEFORE_LIFT:
            return False
        dt = pts[-1][0] - pts[0][0]
        if dt < 0.02:
            return False
        vx = sum(p[1] for p in pts[1:]) / dt        # tablet units / s
        vy = sum(p[2] for p in pts[1:]) / dt
        speed = math.hypot(vx / self.upm_x, vy / self.upm_y)    # mm/s
        if speed < FLICK_MIN_SPEED:
            return False
        if speed > FLICK_MAX_SPEED:
            f = FLICK_MAX_SPEED / speed
            vx, vy = vx * f, vy * f
        self.coast = [-vx * self.kx * self.dir, vy * self.ky * self.dir]   # wheel units / s
        if math.hypot(*self.coast) < COAST_STOP_SPEED:
            return False
        self.coast_t = now
        self.coasting = True
        self.timers["coast"] = now + COAST_STEP
        self._mode_note("glide")
        return True

    def _mode_note(self, text):
        if self.debug:
            print(f"  [{self.clock() % 1000:8.3f}] {text}")

    def _timer_coast(self, now):
        if not self.coasting:
            return
        dt = min(now - self.coast_t, 0.05)
        self.coast_t = now
        decay = math.exp(-KINETIC_FRICTION * dt)
        k = (1.0 - decay) / KINETIC_FRICTION        # integral of v0*exp(-f*t) over dt
        self.vmouse.scroll(self.coast[0] * k, self.coast[1] * k)
        self.coast[0] *= decay
        self.coast[1] *= decay
        if math.hypot(*self.coast) < COAST_STOP_SPEED:
            self._stop_coasting(sync=True)
        else:
            self.timers["coast"] = now + COAST_STEP

    def _stop_coasting(self, sync):
        if not self.coasting:
            return
        self.coasting = False
        self.timers.pop("coast", None)
        if sync:
            self._frame()                   # cursor catches up with the pen

    # ---- shutdown
    def shutdown(self):
        """Release everything the virtual devices may still be holding down."""
        self.timers.clear()
        self.coasting = False
        try:
            self.vmouse.button(e.BTN_LEFT, False)
            self.vmouse.button(e.BTN_MIDDLE, False)
            self.vmouse.button(e.BTN_RIGHT, False)
            self.vpen.send(self.s, None, 0, 0, 0, False)
        except OSError:
            pass


# ------------------------------------------------------------------ device discovery
def is_pen_device(dev):
    """Heuristic: an X/Y device with pen buttons that is not a multitouch screen."""
    caps = dev.capabilities()
    keys = caps.get(e.EV_KEY, [])
    axes = {a[0] if isinstance(a, tuple) else a for a in caps.get(e.EV_ABS, [])}
    return (not dev.name.startswith(VIRTUAL_PREFIX)
            and e.ABS_X in axes and e.ABS_Y in axes
            and (e.BTN_TOOL_PEN in keys or e.BTN_STYLUS in keys)
            and e.ABS_MT_POSITION_X not in axes)          # not a touch device


def list_pens():
    pens = []
    for path in evdev.list_devices():
        try:
            dev = InputDevice(path)
        except OSError:
            continue
        if is_pen_device(dev):
            pens.append(dev)
        else:
            dev.close()
    pens.sort(key=lambda d: d.info.vendor != 0x056A)            # Wacom first
    return pens


def open_pen(spec=None, identity=None):
    """Find the pen to use.
    identity (vendor, product, name) wins: it survives a replug where the kernel
    assigns a new /dev/input/eventN.  spec is a /dev path or a name substring."""
    pens = list_pens()
    match = None
    if identity is not None:
        match = next((d for d in pens
                      if (d.info.vendor, d.info.product, d.name) == identity), None)
    if match is None and spec:
        if spec.startswith("/dev/"):
            match = next((d for d in pens if d.path == spec), None)
        else:
            low = spec.lower()                             # match by name substring
            match = next((d for d in pens if low in d.name.lower()), None)
    if match is None and not spec:                         # plain auto-detect
        match = pens[0] if pens else None
    if match is None and spec and spec.startswith("/dev/") and identity is None:
        try:                                               # exists but failed the pen heuristic
            dev = InputDevice(spec)
            axes = {a[0] if isinstance(a, tuple) else a
                    for a in dev.capabilities().get(e.EV_ABS, [])}
            if e.ABS_X not in axes or e.ABS_Y not in axes:
                print(f"Error: {spec} ({dev.name}) has no pen X/Y axes.")
                dev.close()
                return None
            print(f"Warning: {spec} ({dev.name}) doesn't look like a pen tablet - trying it anyway.")
            return dev
        except OSError:
            return None
    for d in pens:
        if d is not match:
            try:
                d.close()
            except OSError:
                pass
    return match


# ------------------------------------------------------------------ main loop
def run(pen, settings, debug):
    vpen = vmouse = emu = None
    grabbed = False
    try:
        vpen = VirtualPen(pen)
        vmouse = VirtualMouse()
        emu = TouchEmulator(pen, vpen, vmouse, settings["brush"], debug=debug)
        pen.grab()
        grabbed = True
        w, h = emu.size_mm
        print("=" * 66)
        print(f" Connected to: {pen.name} ({pen.path})   active area {w:.0f} x {h:.0f} mm")
        print(" TOUCH MODE : tap = click | drag = scroll (flick = glide)")
        print(f"              hold {HOLD_SELECT_TIME:.2f}s + drag = select | "
              f"hold {LONG_PRESS_RCLICK:.2f}s = right-click")
        if TAP_DRAG_SELECT:
            print("              tap, then drag = select (like Windows Ink)")
        print(" Side button 1 (held)  : that stroke acts as a normal pen")
        print(" Side button 2 (click) : toggle BRUSH MODE (full pen passthrough)")
        print(" Now in: " + ("BRUSH MODE" if emu.brush_mode else "TOUCH MODE"))
        print(" Ctrl+C to quit")
        print("=" * 66)
        while True:
            deadline = emu.next_deadline()
            timeout = None if deadline is None else max(0.0, deadline - emu.clock())
            readable, _, _ = select.select([pen.fd], [], [], timeout)
            if readable:
                try:
                    for ev in pen.read():
                        emu.feed(ev)
                except BlockingIOError:
                    pass
            emu.tick()
    finally:
        if emu:
            settings["brush"] = emu.brush_mode      # remember across a reconnect
            emu.shutdown()
        if grabbed:
            try:
                pen.ungrab()
            except OSError:
                pass
        for dev in (vpen, vmouse):
            if dev:
                try:
                    dev.close()
                except OSError:
                    pass


def main():
    ap = argparse.ArgumentParser(description="Make a pen tablet behave like a touch screen.")
    ap.add_argument("--device", help="pen device: /dev/input/eventN or a name substring (default: auto-detect)")
    ap.add_argument("--list", action="store_true", help="list detected pen devices and exit")
    ap.add_argument("--brush", action="store_true", help="start in brush mode")
    ap.add_argument("--debug", action="store_true", help="print gesture state changes")
    ap.add_argument("--speed", type=float,
                    help=f"scroll speed, notches per full-area drag (default {SCROLL_SPEED})")
    ap.add_argument("--select-time", type=float, dest="select_time",
                    help=f"hold this long, then drag = select (default {HOLD_SELECT_TIME})")
    ap.add_argument("--rclick-time", type=float, dest="rclick_time",
                    help=f"hold still this long = right click (default {LONG_PRESS_RCLICK})")
    ap.add_argument("--click-pressure", type=float, dest="click_pressure",
                    help=f"synthetic tip pressure, 0..1 (default {CLICK_PRESSURE})")
    ap.add_argument("--natural", dest="natural", action="store_true", default=None,
                    help="content follows the pen while scrolling (default)")
    ap.add_argument("--no-natural", dest="natural", action="store_false", default=None,
                    help="classic wheel direction")
    ap.add_argument("--kinetic", dest="kinetic", action="store_true", default=None,
                    help="keep gliding after a flick (default)")
    ap.add_argument("--no-kinetic", dest="kinetic", action="store_false", default=None,
                    help="no glide after a flick")
    ap.add_argument("--axis-lock", dest="axis_lock", action="store_true", default=None,
                    help="lock scroll drags to one axis (default)")
    ap.add_argument("--no-axis-lock", dest="axis_lock", action="store_false", default=None,
                    help="free diagonal scrolling")
    ap.add_argument("--tap-drag", dest="tap_drag", action="store_true", default=None,
                    help="tap, then drag = select (default)")
    ap.add_argument("--no-tap-drag", dest="tap_drag", action="store_false", default=None,
                    help="tap only clicks; a drag always scrolls")
    args = ap.parse_args()

    if args.list:
        pens = list_pens()
        for d in pens:
            print(f"{d.path}\t{d.name}\t{d.info.vendor:04x}:{d.info.product:04x}")
            d.close()
        if not pens:
            note = "" if os.geteuid() == 0 else "  (try with sudo)"
            print("No pen tablets found." + note)
        return

    if os.geteuid() != 0:
        sys.exit(f"Error: permission denied.\nRun with sudo:\n  sudo {sys.executable} {sys.argv[0]}")

    if not os.path.exists("/dev/uinput"):
        sys.exit("Error: /dev/uinput not found.\n"
                 "Load the uinput module:  sudo modprobe uinput")

    g = globals()                    # apply overrides BEFORE the emulator is built
    if args.speed is not None:
        g["SCROLL_SPEED"] = args.speed
    if args.select_time is not None:
        g["HOLD_SELECT_TIME"] = args.select_time
    if args.rclick_time is not None:
        g["LONG_PRESS_RCLICK"] = args.rclick_time
    if args.click_pressure is not None:
        g["CLICK_PRESSURE"] = min(1.0, max(0.05, args.click_pressure))
    if args.natural is not None:
        g["NATURAL_SCROLL"] = args.natural
    if args.kinetic is not None:
        g["KINETIC_SCROLL"] = args.kinetic
    if args.axis_lock is not None:
        g["AXIS_LOCK"] = args.axis_lock
    if args.tap_drag is not None:
        g["TAP_DRAG_SELECT"] = args.tap_drag
    if HOLD_SELECT_TIME >= LONG_PRESS_RCLICK:
        print("Warning: --select-time >= --rclick-time; hold-then-drag will never trigger.")

    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    settings = {"brush": args.brush or START_IN_BRUSH_MODE}
    identity = None
    waiting = False
    try:
        while True:
            pen = open_pen(args.device, identity)
            if pen is None:
                if not waiting:
                    print("No pen tablet found - waiting for one (Ctrl+C to quit)...")
                    waiting = True
                time.sleep(1.0)
                continue
            waiting = False
            if identity is None:
                identity = (pen.info.vendor, pen.info.product, pen.name)
            try:
                run(pen, settings, args.debug)
            except UInputError as exc:
                sys.exit(f"Error: cannot create virtual devices ({exc}).\n"
                         "Is the uinput module loaded?  sudo modprobe uinput")
            except OSError as exc:
                if exc.errno == errno.EBUSY:
                    sys.exit("Error: the pen is already grabbed by another program "
                             "(a second copy of this script?).")
                if exc.errno not in (errno.ENODEV, errno.EIO, errno.ENOENT):
                    raise
                print("Tablet disconnected - waiting for it to come back...")
                waiting = True
            finally:
                pen.close()
            time.sleep(0.5)
    except KeyboardInterrupt:
        print("\nStopping...")


if __name__ == "__main__":
    main()