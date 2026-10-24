#!/usr/bin/env python3
# A native Wayland window with one text field. Its text goes to the file
# named on the command line after every change; "<file>.focused" exists
# while the field has keyboard focus.
import os
import sys

import gi

gi.require_version("Gtk", "3.0")
from gi.repository import Gtk  # noqa: E402

out = sys.argv[1]


def changed(buf):
    text = buf.get_text(buf.get_start_iter(), buf.get_end_iter(), True)
    with open(out + ".tmp", "w") as f:
        f.write(text)
    os.replace(out + ".tmp", out)


def focused(*_):
    open(out + ".focused", "w").close()
    return False


def unfocused(*_):
    if os.path.exists(out + ".focused"):
        os.remove(out + ".focused")
    return False


win = Gtk.Window(title="capture " + os.path.basename(out))
view = Gtk.TextView()
view.get_buffer().connect("changed", changed)
view.connect("focus-in-event", focused)
view.connect("focus-out-event", unfocused)
win.add(view)
win.set_default_size(700, 300)
win.connect("destroy", Gtk.main_quit)
win.show_all()
view.grab_focus()
Gtk.main()
