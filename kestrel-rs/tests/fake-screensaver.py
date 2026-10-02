#!/usr/bin/env python3
"""A stand-in for the desktop's screensaver, for testing src/power.rs.

Kestrel asks the desktop to stop blanking the screen while it is fullscreen,
over `org.freedesktop.ScreenSaver`. That path cannot be exercised on a build
machine with no desktop on it, and getting it wrong is invisible — the app
carries on working and the screen goes black half an hour later.

So this claims the name and answers the two calls, printing what it was asked.
The point is the printing: it shows the inhibit arriving, the cookie going back,
and the matching release, which is the whole of the contract.

    dbus-run-session -- sh -c '
        python3 tests/fake-screensaver.py &
        sleep 1
        DISPLAY=:0 cargo test -- --ignored --nocapture live_inhibit
    '
"""

import sys

import gi

gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib  # noqa: E402

INTERFACE = """
<node>
  <interface name='org.freedesktop.ScreenSaver'>
    <method name='Inhibit'>
      <arg type='s' name='application_name' direction='in'/>
      <arg type='s' name='reason_for_inhibit' direction='in'/>
      <arg type='u' name='cookie' direction='out'/>
    </method>
    <method name='UnInhibit'>
      <arg type='u' name='cookie' direction='in'/>
    </method>
  </interface>
</node>
"""

held = {}
next_cookie = [1]


def on_call(_conn, _sender, _path, _iface, method, params, invocation):
    if method == "Inhibit":
        application, reason = params.unpack()
        cookie = next_cookie[0]
        next_cookie[0] += 1
        held[cookie] = (application, reason)
        print(f"  Inhibit({application!r}, {reason!r}) -> cookie {cookie}", flush=True)
        invocation.return_value(GLib.Variant("(u)", (cookie,)))
    elif method == "UnInhibit":
        (cookie,) = params.unpack()
        was = held.pop(cookie, None)
        print(
            f"  UnInhibit({cookie})"
            + ("" if was else "  -- for a cookie never issued!"),
            flush=True,
        )
        # Held inhibits left over at exit would mean a release went missing.
        print(f"  still held: {sorted(held)}", flush=True)
        invocation.return_value(None)
    else:
        invocation.return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod", method)


def main():
    connection = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    node = Gio.DBusNodeInfo.new_for_xml(INTERFACE)
    connection.register_object(
        "/org/freedesktop/ScreenSaver",
        node.interfaces[0],
        on_call,
        None,
        None,
    )

    def acquired(*_args):
        print("fake screensaver: holding org.freedesktop.ScreenSaver", flush=True)

    def lost(*_args):
        print("fake screensaver: could not take the name", flush=True)
        sys.exit(1)

    Gio.bus_own_name_on_connection(
        connection,
        "org.freedesktop.ScreenSaver",
        Gio.BusNameOwnerFlags.NONE,
        acquired,
        lost,
    )
    GLib.MainLoop().run()


if __name__ == "__main__":
    main()
