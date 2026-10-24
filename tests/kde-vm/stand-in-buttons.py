# Stand-ins for dictation buttons that are no keyboard keys, like
# stand-in-keyboard.py: a device appears, waits for readers to find it,
# does what the arguments say and goes away.
#
#   stand-in-buttons mouse WAIT [+BTN_X | -BTN_X | BTN_X | SECONDS]...
#     a uinput mouse with BTN_TASK, as on the Logitech receiver
#   stand-in-buttons speaker WAIT [HEX | SECONDS]...
#     a HID device through /dev/uhid with a report 3 whose first byte's
#     bit 0 is a Telephony hook switch, like the Jabra SPEAK 410's call
#     button; HEX is one raw input report, its id first ("0301").
#
# SECONDS waits and has a point ("0.5").
import os
import struct
import sys
import time

kind, wait, steps = sys.argv[1], float(sys.argv[2]), sys.argv[3:]

if kind == "mouse":
    from evdev import UInput, ecodes

    ui = UInput(
        {
            ecodes.EV_KEY: [ecodes.BTN_LEFT, ecodes.BTN_RIGHT, ecodes.BTN_TASK],
            ecodes.EV_REL: [ecodes.REL_X, ecodes.REL_Y],
        },
        name="stand-in mouse",
    )

    def step(arg):
        value = {"+": 1, "-": 0}.get(arg[0])
        name = arg[1:] if value is not None else arg
        for v in [value] if value is not None else [1, 0]:
            ui.write(ecodes.EV_KEY, ecodes.ecodes[name], v)
            ui.syn()

    close = ui.close
else:
    # <linux/uhid.h>: struct uhid_event is a u32 type and a union whose
    # largest member is uhid_create2_req.
    UHID_DESTROY, UHID_CREATE2, UHID_INPUT2 = 1, 11, 12
    EVENT_SIZE = 4 + 128 + 64 + 64 + 2 + 2 + 4 * 4 + 4096
    DESCRIPTOR = bytes(
        [
            0x05, 0x0B,  # Usage Page (Telephony)
            0x09, 0x05,  # Usage (Headset)
            0xA1, 0x01,  # Collection (Application)
            0x85, 0x03,  # Report ID (3)
            0x09, 0x20,  # Usage (Hook Switch)
            0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x01,
            0x81, 0x02,  # Input (Data, Variable, Absolute): 1 bit
            0x75, 0x07, 0x95, 0x01,
            0x81, 0x03,  # Input (Constant): 7 bits padding
            0xC0,  # End Collection
        ]
    )
    fd = os.open("/dev/uhid", os.O_RDWR)

    def send(kind, body):
        os.write(fd, struct.pack("<I", kind) + body.ljust(EVENT_SIZE - 4, b"\0"))

    # USB, pid.codes' test vendor:product 1209:0001.
    send(
        UHID_CREATE2,
        struct.pack(
            "<128s64s64sHHIIII4096s",
            b"stand-in speaker",
            b"stand-in",
            b"",
            len(DESCRIPTOR),
            0x03,
            0x1209,
            0x0001,
            0,
            0,
            DESCRIPTOR,
        ),
    )

    def step(arg):
        report = bytes.fromhex(arg)
        send(UHID_INPUT2, struct.pack("<H", len(report)) + report)

    def close():
        send(UHID_DESTROY, b"")
        os.close(fd)


time.sleep(wait)
for arg in steps:
    if arg[0].isdigit() and "." in arg:
        time.sleep(float(arg))
    else:
        step(arg)
    time.sleep(0.02)
time.sleep(0.3)
close()
