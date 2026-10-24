# A stand-in for a real keyboard: a uinput device that appears, waits for
# readers to find it, presses the keys given as arguments and goes away.
# KEY_X taps a key, +KEY_X holds it down, -KEY_X lets go, a number waits
# that many seconds.
import sys
import time

from evdev import UInput, ecodes

ui = UInput({ecodes.EV_KEY: list(range(1, 248))}, name="stand-in keyboard")
time.sleep(float(sys.argv[1]))


def key(name, value):
    ui.write(ecodes.EV_KEY, ecodes.ecodes[name], value)
    ui.syn()


for arg in sys.argv[2:]:
    if arg[0] == "+":
        key(arg[1:], 1)
    elif arg[0] == "-":
        key(arg[1:], 0)
    elif arg[0].isdigit():
        time.sleep(float(arg))
    else:
        key(arg, 1)
        key(arg, 0)
    time.sleep(0.02)
time.sleep(0.3)
ui.close()
