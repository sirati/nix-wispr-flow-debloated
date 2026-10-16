# Mouse and speaker buttons that start/stop dictation in Wispr Flow.
#
# Wispr takes its push-to-talk from key events: its helper reads
# /dev/input/event* and ignores everything but keyboards. So a button press is
# turned into a key press on a virtual keyboard, and that key is then bound in
# Wispr (Settings > Shortcuts, hands-free mode; the recorder accepts the next
# press of the button).
#
# The helper lists keyboards once, when it starts, and does not notice devices
# that appear later. That is why this is a system service that creates its
# device at boot, before anyone logs in, instead of a user service. Restarting
# the service (it is restarted by a switch that changes it) creates a new
# device node, which a running Wispr will not see until it is restarted. The
# helper also only counts a device as a keyboard if it advertises KEY_A..KEY_Z,
# which is why the virtual device does.
#
# The default key is F19: a plain keysym that GNOME does not bind. F13-F18 are
# XF86Tools/Launch5-9 and F20 is XF86AudioMicMute, which would mute the mic.
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.dictationButtons;

  buttons = pkgs.writeText "dictation-buttons.json" (
    builtins.toJSON {
      inherit (cfg) evdev hidraw;
    }
  );

  listener =
    pkgs.writers.writePython3Bin "dictation-buttons"
      {
        libraries = [ pkgs.python3Packages.evdev ];
        flakeIgnore = [ "E501" ];
      }
      ''
        import asyncio
        import glob
        import json
        import os
        import string

        import evdev
        from evdev import UInput, ecodes

        with open(os.environ["DICTATION_BUTTONS"]) as f:
            CONFIG = json.load(f)
        EVDEV = CONFIG["evdev"]
        HIDRAW = CONFIG["hidraw"]
        TOGGLE_KEY = ecodes.ecodes[os.environ["TOGGLE_KEY"]]

        # Wispr's helper only treats a device as a keyboard if it has A and Z.
        LETTERS = [ecodes.ecodes["KEY_" + c] for c in string.ascii_uppercase]
        KEYBOARD = UInput(
            {ecodes.EV_KEY: LETTERS + [TOGGLE_KEY]},
            name="dictation-buttons",
        )


        def toggle():
            KEYBOARD.write(ecodes.EV_KEY, TOGGLE_KEY, 1)
            KEYBOARD.syn()
            KEYBOARD.write(ecodes.EV_KEY, TOGGLE_KEY, 0)
            KEYBOARD.syn()


        def key_names(code):
            name = ecodes.bytype[ecodes.EV_KEY].get(code, str(code))
            return name if isinstance(name, list) else [name]


        def hidraw_devices():
            found = {}
            for uevent in glob.glob("/sys/class/hidraw/hidraw*/device/uevent"):
                node = "/dev/" + uevent.split("/")[4]
                with open(uevent) as f:
                    for line in f:
                        if line.startswith("HID_NAME="):
                            found[node] = line.split("=", 1)[1].strip()
            return found


        async def watch(path, name, codes):
            print("watching", name, "at", path, flush=True)
            try:
                dev = evdev.InputDevice(path)
                async for event in dev.async_read_loop():
                    if event.type != ecodes.EV_KEY or event.value != 1:
                        continue
                    names = key_names(event.code)
                    print(name, "pressed", names, flush=True)
                    if codes.intersection(names):
                        toggle()
            except OSError as err:
                # Unplugged; the scan loop re-attaches when it returns.
                print("lost", name, err, flush=True)


        async def watch_hidraw(path, name, specs):
            loop = asyncio.get_running_loop()
            try:
                fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK)
            except OSError as err:
                print("cannot open", path, "for", name, err, flush=True)
                # Back off so the scan loop does not retry every pass.
                await asyncio.sleep(60)
                return
            print("watching raw reports of", name, "at", path, flush=True)
            previous = {}
            done = loop.create_future()

            def handle(data):
                report, body = data[0], data[1:]
                old = previous.get(report, bytes(len(body)))
                previous[report] = body
                for index, (before, now) in enumerate(zip(old, body), start=1):
                    for bit in range(8):
                        if (before >> bit) & 1 or not (now >> bit) & 1:
                            continue
                        spec = "%d:%d:%d" % (report, index, bit)
                        print(name, "bit rose", spec, flush=True)
                        if spec in specs:
                            toggle()

            def on_readable():
                try:
                    data = os.read(fd, 64)
                except BlockingIOError:
                    return
                except OSError as err:
                    if not done.done():
                        done.set_result(err)
                    return
                if data:
                    handle(data)
                elif not done.done():
                    # EOF: the device went away. Stop polling it.
                    done.set_result(EOFError("end of file"))

            loop.add_reader(fd, on_readable)
            err = await done
            loop.remove_reader(fd)
            os.close(fd)
            print("lost", name, err, flush=True)


        async def main():
            tasks = {}
            while True:
                for path in evdev.list_devices():
                    if path in tasks and not tasks[path].done():
                        continue
                    try:
                        dev = evdev.InputDevice(path)
                        name = dev.name
                        dev.close()
                    except OSError:
                        continue
                    if name in EVDEV:
                        tasks[path] = asyncio.create_task(
                            watch(path, name, set(EVDEV[name]))
                        )
                for path, name in hidraw_devices().items():
                    if path in tasks and not tasks[path].done():
                        continue
                    if name in HIDRAW:
                        tasks[path] = asyncio.create_task(
                            watch_hidraw(path, name, set(HIDRAW[name]))
                        )
                await asyncio.sleep(3)


        asyncio.run(main())
      '';
in
{
  options.services.dictationButtons = {
    enable = lib.mkEnableOption "buttons that toggle dictation by pressing a virtual key";

    key = lib.mkOption {
      type = lib.types.str;
      default = "KEY_F19";
      description = "evdev key name the virtual keyboard presses; bind it in Wispr.";
    };

    evdev = lib.mkOption {
      type = lib.types.attrsOf (lib.types.listOf lib.types.str);
      default = { };
      example = {
        "Logitech USB Receiver Mouse" = [ "BTN_TASK" ];
      };
      description = ''
        Key names per input device name. Pick codes GNOME does not act on:
        BTN_TASK is a spare mouse button.
      '';
    };

    hidraw = lib.mkOption {
      type = lib.types.attrsOf (lib.types.listOf lib.types.str);
      default = { };
      example = {
        "Jabra SPEAK 410 USB" = [ "3:1:0" ];
      };
      description = ''
        "report:byte:bit" strings per HID device name, toggling when the bit
        goes 0 -> 1 (byte 1 is the first byte after the report id). The
        service logs "bit rose R:B:b" for every edge, so wrong bindings can be
        corrected from `journalctl -u dictation-buttons`. The Jabra's call and
        hang-up buttons are Telephony and vendor-defined usages that the
        kernel's hid-jabra driver drops, so they never become key events; they
        are read from the raw HID node. Measured on the Jabra SPEAK 410: report
        1 bits 0/1/2 are volume down/up/mute (also visible via evdev); report 3
        bit 0 is the Telephony hook switch, which the call/hang-up button sends
        as a momentary 1 then 0.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.services.dictation-buttons = {
      description = "Toggle dictation from mouse and speaker buttons";
      wantedBy = [ "multi-user.target" ];
      environment = {
        DICTATION_BUTTONS = "${buttons}";
        TOGGLE_KEY = cfg.key;
      };
      serviceConfig = {
        ExecStart = "${listener}/bin/dictation-buttons";
        Restart = "on-failure";
        RestartSec = 5;
        # /dev/uinput and the input nodes are group `input` (mode 0660) through
        # Wispr's udev rule; the hidraw node through the rule on the host.
        DynamicUser = true;
        SupplementaryGroups = [ "input" ];
        DevicePolicy = "closed";
        DeviceAllow = [
          "/dev/uinput rw"
          "char-input rw"
          "char-hidraw rw"
        ];
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
      };
    };
  };
}
