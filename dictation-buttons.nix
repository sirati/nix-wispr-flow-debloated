# services.dictationButtons is gone: it pressed F19 on a virtual keyboard for
# Wispr's own shortcut, which Wispr's shortcut recorder never accepted.
# wispr-headless now reads such buttons itself, as hotkeys. Kept as a module
# so a configuration that still sets the option is told what to use instead.
{ lib, ... }:
{
  imports = [
    (lib.mkRemovedOptionModule [ "services" "dictationButtons" ] ''
      Bind the buttons in programs.wisprFlow.headless.hotkeys instead: a mouse
      button by its evdev name ("wispr:BTN_TASK"), a button that only shows in
      raw HID reports as hid:DEVICE/REPORT:BYTE:BIT with the same
      report:byte:bit as before ("wispr:toggle:hid:0b0e:0412/3:1:0"). The
      udev rule for the hidraw node of a vendor:product button comes with it.
    '')
  ];
}
