#!/usr/bin/env python3
import os
import sys
import time
import select
import evdev
from evdev import ecodes, InputDevice, UInput, AbsInfo

# --- TUNABLE SETTINGS ---
LONG_PRESS_TIME = 0.50   # Seconds to hold still for Long-Press (Text handles / Right-click)
DEADZONE_RATIO = 0.006   # Movement allowed during long-press (0.6% of tablet width)

def find_wacom_pen():
    if os.geteuid() != 0:
        print("Error: Permission denied.")
        print(f"Please run with sudo:\n  sudo /usr/bin/python {sys.argv[0]}")
        sys.exit(1)

    for path in evdev.list_devices():
        dev = InputDevice(path)
        if "Virtual Wacom" in dev.name:
            continue
        caps = dev.capabilities()
        keys = caps.get(ecodes.EV_KEY, [])
        abs_axes = [a[0] if isinstance(a, tuple) else a for a in caps.get(ecodes.EV_ABS, [])]

        if ecodes.ABS_X in abs_axes and ecodes.ABS_Y in abs_axes:
            if ecodes.BTN_TOOL_PEN in keys or "pen" in dev.name.lower() or "stylus" in dev.name.lower():
                if "pad" not in dev.name.lower() and "finger" not in dev.name.lower():
                    return dev
    return None

def main():
    pen = find_wacom_pen()
    if not pen:
        print("Error: No Wacom Pen found. Unplug and replug the tablet.")
        sys.exit(1)

    print(f"Connected to: {pen.name} ({pen.path})")

    abs_x = pen.absinfo(ecodes.ABS_X)
    abs_y = pen.absinfo(ecodes.ABS_Y)
    x_range = max(1, abs_x.max - abs_x.min)
    deadzone = x_range * DEADZONE_RATIO

    # 1. Virtual Stylus (for visible hover cursor & Photoshop Brush Mode)
    ui_pen = UInput.from_device(pen, name="Virtual Wacom Ink Stylus")

    # 2. True Virtual Touchscreen (INPUT_PROP_DIRECT = 0x01)
    # Gives native Wayland wl_touch: smooth finger scrolling + Android text selection handles
    touch_caps = {
        ecodes.EV_KEY: [ecodes.BTN_TOUCH],
        ecodes.EV_ABS: [
            (ecodes.ABS_X, abs_x),
            (ecodes.ABS_Y, abs_y),
            (ecodes.ABS_MT_SLOT, AbsInfo(0, 0, 1, 0, 0, 0)),
            (ecodes.ABS_MT_TRACKING_ID, AbsInfo(0, 0, 65535, 0, 0, 0)),
            (ecodes.ABS_MT_POSITION_X, abs_x),
            (ecodes.ABS_MT_POSITION_Y, abs_y),
        ],
    }
    try:
        ui_touch = UInput(touch_caps, name="Virtual Wacom Ink Touchscreen", input_props=[ecodes.INPUT_PROP_DIRECT])
    except TypeError:
        ui_touch = UInput(touch_caps, name="Virtual Wacom Ink Touchscreen")

    # 3. Auxiliary Mouse Button device (for crisp Right-Click on long-press)
    mouse_caps = {
        ecodes.EV_KEY: [ecodes.BTN_LEFT, ecodes.BTN_RIGHT, ecodes.BTN_MIDDLE],
        ecodes.EV_REL: [ecodes.REL_X, ecodes.REL_Y],
    }
    ui_mouse = UInput(mouse_caps, name="Virtual Wacom Ink Mouse")

    pen.grab()
    print("=========================================================")
    print(" ACTIVE MODE: TOUCH MODE (Browser / Android-style Touch)")
    print(" - Drag pen: Native Touchscreen Scroll & Drag Selection Handles")
    print(" - Hold pen still (0.5s): Long-Press Text Selection / Right-Click")
    print(" - Hold Side Button 1 + Drag: Instant Pen / Brush / Select")
    print(" - Click Side Button 2: Toggle BRUSH MODE (for Photoshop)")
    print("=========================================================")

    cur_x, cur_y, cur_p = abs_x.value, abs_y.value, 0
    cur_tilt_x, cur_tilt_y = 0, 0
    touch = False
    prev_touch = False
    btn_stylus1 = False
    btn_stylus2 = False
    prev_btn2 = False

    brush_mode = False       # Toggled by Side Button 2
    stroke_type = "NONE"     # "TOUCH" or "PEN"
    start_x, start_y = 0, 0
    touch_start_time = 0.0
    long_press_fired = False
    tracking_id = 1

    def send_touch_down(x, y):
        nonlocal tracking_id
        tracking_id = (tracking_id + 1) % 60000 + 1
        # Retract stylus hover so compositor doesn't block touch events
        ui_pen.write(ecodes.EV_KEY, ecodes.BTN_TOOL_PEN, 0)
        ui_pen.syn()
        # Emit multi-touch finger down
        ui_touch.write(ecodes.EV_ABS, ecodes.ABS_MT_SLOT, 0)
        ui_touch.write(ecodes.EV_ABS, ecodes.ABS_MT_TRACKING_ID, tracking_id)
        ui_touch.write(ecodes.EV_ABS, ecodes.ABS_MT_POSITION_X, x)
        ui_touch.write(ecodes.EV_ABS, ecodes.ABS_MT_POSITION_Y, y)
        ui_touch.write(ecodes.EV_ABS, ecodes.ABS_X, x)
        ui_touch.write(ecodes.EV_ABS, ecodes.ABS_Y, y)
        ui_touch.write(ecodes.EV_KEY, ecodes.BTN_TOUCH, 1)
        ui_touch.syn()

    def send_touch_move(x, y):
        ui_touch.write(ecodes.EV_ABS, ecodes.ABS_MT_SLOT, 0)
        ui_touch.write(ecodes.EV_ABS, ecodes.ABS_MT_POSITION_X, x)
        ui_touch.write(ecodes.EV_ABS, ecodes.ABS_MT_POSITION_Y, y)
        ui_touch.write(ecodes.EV_ABS, ecodes.ABS_X, x)
        ui_touch.write(ecodes.EV_ABS, ecodes.ABS_Y, y)
        ui_touch.syn()

    def send_touch_up():
        ui_touch.write(ecodes.EV_ABS, ecodes.ABS_MT_SLOT, 0)
        ui_touch.write(ecodes.EV_ABS, ecodes.ABS_MT_TRACKING_ID, -1)
        ui_touch.write(ecodes.EV_KEY, ecodes.BTN_TOUCH, 0)
        ui_touch.syn()
        # Restore stylus hover cursor
        ui_pen.write(ecodes.EV_KEY, ecodes.BTN_TOOL_PEN, 1)
        ui_pen.write(ecodes.EV_ABS, ecodes.ABS_X, cur_x)
        ui_pen.write(ecodes.EV_ABS, ecodes.ABS_Y, cur_y)
        ui_pen.write(ecodes.EV_ABS, ecodes.ABS_PRESSURE, 0)
        ui_pen.syn()

    try:
        while True:
            # Wait up to 20ms so we can check the long-press timer even if the pen is held dead still
            r, _, _ = select.select([pen.fd], [], [], 0.02)

            if r:
                for event in pen.read():
                    if event.type == ecodes.EV_ABS:
                        if event.code == ecodes.ABS_X:
                            cur_x = event.value
                        elif event.code == ecodes.ABS_Y:
                            cur_y = event.value
                        elif event.code == ecodes.ABS_PRESSURE:
                            cur_p = event.value
                        elif event.code == ecodes.ABS_TILT_X:
                            cur_tilt_x = event.value
                        elif event.code == ecodes.ABS_TILT_Y:
                            cur_tilt_y = event.value

                    elif event.type == ecodes.EV_KEY:
                        if event.code == ecodes.BTN_TOUCH:
                            touch = (event.value == 1)
                        elif event.code == ecodes.BTN_STYLUS:
                            btn_stylus1 = (event.value == 1)
                        elif event.code == ecodes.BTN_STYLUS2:
                            btn_stylus2 = (event.value == 1)
                            if btn_stylus2 and not prev_btn2 and not touch:
                                brush_mode = not brush_mode
                                state_name = "BRUSH / PHOTOSHOP MODE (Full Pen & Pressure)" if brush_mode else "TOUCH MODE (Finger Scroll & Text Handles)"
                                print(f"\n>>> Switched to: {state_name} <<<")
                            prev_btn2 = btn_stylus2

                    elif event.type == ecodes.EV_SYN and event.code == ecodes.SYN_REPORT:
                        now = time.monotonic()

                        # 1. Pen just touched surface
                        if touch and not prev_touch:
                            start_x, start_y = cur_x, cur_y
                            touch_start_time = now
                            long_press_fired = False

                            if brush_mode or btn_stylus1:
                                stroke_type = "PEN"
                                ui_pen.write(ecodes.EV_KEY, ecodes.BTN_TOOL_PEN, 1)
                                ui_pen.write(ecodes.EV_ABS, ecodes.ABS_X, cur_x)
                                ui_pen.write(ecodes.EV_ABS, ecodes.ABS_Y, cur_y)
                                ui_pen.write(ecodes.EV_ABS, ecodes.ABS_PRESSURE, cur_p)
                                ui_pen.write(ecodes.EV_KEY, ecodes.BTN_TOUCH, 1)
                                ui_pen.syn()
                            else:
                                stroke_type = "TOUCH"
                                send_touch_down(cur_x, cur_y)

                        # 2. Pen is moving/held on surface
                        elif touch and prev_touch:
                            if stroke_type == "PEN":
                                ui_pen.write(ecodes.EV_ABS, ecodes.ABS_X, cur_x)
                                ui_pen.write(ecodes.EV_ABS, ecodes.ABS_Y, cur_y)
                                ui_pen.write(ecodes.EV_ABS, ecodes.ABS_PRESSURE, cur_p)
                                ui_pen.write(ecodes.EV_ABS, ecodes.ABS_TILT_X, cur_tilt_x)
                                ui_pen.write(ecodes.EV_ABS, ecodes.ABS_TILT_Y, cur_tilt_y)
                                ui_pen.syn()
                            elif stroke_type == "TOUCH" and not long_press_fired:
                                dist = ((cur_x - start_x) ** 2 + (cur_y - start_y) ** 2) ** 0.5
                                # Keep touch stationary inside deadzone so long-press doesn't cancel
                                if dist >= deadzone:
                                    send_touch_move(cur_x, cur_y)

                        # 3. Pen lifted off surface
                        elif not touch and prev_touch:
                            if stroke_type == "PEN":
                                ui_pen.write(ecodes.EV_ABS, ecodes.ABS_PRESSURE, 0)
                                ui_pen.write(ecodes.EV_KEY, ecodes.BTN_TOUCH, 0)
                                ui_pen.syn()
                            elif stroke_type == "TOUCH":
                                if not long_press_fired:
                                    send_touch_up()
                            stroke_type = "NONE"

                        # 4. Hovering above tablet (NEVER send pressure > 0 here!)
                        else:
                            ui_pen.write(ecodes.EV_KEY, ecodes.BTN_TOOL_PEN, 1)
                            ui_pen.write(ecodes.EV_ABS, ecodes.ABS_X, cur_x)
                            ui_pen.write(ecodes.EV_ABS, ecodes.ABS_Y, cur_y)
                            ui_pen.write(ecodes.EV_ABS, ecodes.ABS_PRESSURE, 0)
                            ui_pen.write(ecodes.EV_KEY, ecodes.BTN_TOUCH, 0)
                            ui_pen.syn()

                        prev_touch = touch

            # Timer check: Fire Long-Press if held still in TOUCH mode for LONG_PRESS_TIME
            if touch and stroke_type == "TOUCH" and not long_press_fired:
                now = time.monotonic()
                dist = ((cur_x - start_x) ** 2 + (cur_y - start_y) ** 2) ** 0.5
                if (now - touch_start_time) >= LONG_PRESS_TIME and dist < deadzone:
                    long_press_fired = True
                    # Release virtual finger after 0.5s (triggers native browser touch-selection handles)
                    # and emit Right-Click for desktop apps that don't support wl_touch long-press
                    send_touch_up()
                    ui_mouse.write(ecodes.EV_KEY, ecodes.BTN_RIGHT, 1)
                    ui_mouse.syn()
                    time.sleep(0.02)
                    ui_mouse.write(ecodes.EV_KEY, ecodes.BTN_RIGHT, 0)
                    ui_mouse.syn()

    except KeyboardInterrupt:
        print("\nStopping Windows Ink daemon...")
    finally:
        pen.ungrab()
        ui_pen.close()
        ui_touch.close()
        ui_mouse.close()

if __name__ == "__main__":
    main()