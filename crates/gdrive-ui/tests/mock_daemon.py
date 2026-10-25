#!/usr/bin/env python3
"""Read-only UI fixture. Run ONLY inside a private dbus-run-session."""

import os
import signal
import sys
import threading

import gi

gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib


XML = """
<node>
  <interface name="org.gclient.GDrive1">
    <method name="ListSyncFolders">
      <arg direction="out" type="a(sssssssbb)"/>
    </method>
    <method name="IsAuthenticated"><arg direction="out" type="b"/></method>
    <signal name="AuthenticationChanged"><arg type="b"/></signal>
    <method name="ListDriveFolders">
      <arg direction="in" type="s"/><arg direction="out" type="a(ss)"/>
    </method>
  </interface>
  <interface name="org.gclient.TestDrive">
    <method name="SetMode"><arg direction="in" type="s"/></method>
    <method name="SetAuthenticated"><arg direction="in" type="b"/></method>
    <method name="PauseAuthenticationReads"><arg direction="in" type="b"/></method>
  </interface>
  <interface name="org.freedesktop.portal.Settings">
    <method name="Read">
      <arg direction="in" type="s"/><arg direction="in" type="s"/>
      <arg direction="out" type="v"/>
    </method>
  </interface>
  <interface name="org.a11y.Bus">
    <method name="GetAddress"><arg direction="out" type="s"/></method>
  </interface>
</node>
"""


def main():
    if os.environ.get("GDRIVE_UI_TEST_BUS") != "1":
        sys.exit("Refusing to run without GDRIVE_UI_TEST_BUS=1 on a private bus")
    if not os.environ.get("DBUS_SESSION_BUS_ADDRESS"):
        sys.exit("Run this fixture inside dbus-run-session")
    mode = "stall"
    authenticated = True
    pause_authentication = False
    authentication_reads = []
    stalled = []
    loop = GLib.MainLoop()
    cancellable = Gio.Cancellable()
    connect_timer = threading.Timer(3, cancellable.cancel)
    connect_timer.start()
    try:
        connection = Gio.DBusConnection.new_for_address_sync(
            os.environ["DBUS_SESSION_BUS_ADDRESS"],
            Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT
            | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION,
            None,
            cancellable,
        )
    finally:
        connect_timer.cancel()

    def call(_connection, _sender, _path, interface, method, parameters, invocation):
        nonlocal mode, authenticated, pause_authentication
        if interface == "org.freedesktop.portal.Settings":
            invocation.return_value(GLib.Variant("(v)", (GLib.Variant("v", GLib.Variant("u", 1)),)))
        elif interface == "org.a11y.Bus":
            invocation.return_value(GLib.Variant("(s)", (os.environ["AT_SPI_BUS_ADDRESS"],)))
        elif interface == "org.gclient.TestDrive":
            if method == "SetAuthenticated":
                authenticated = parameters.unpack()[0]
                connection.emit_signal(
                    None, "/org/gclient/GDrive1", "org.gclient.GDrive1",
                    "AuthenticationChanged", GLib.Variant("(b)", (authenticated,)),
                )
                invocation.return_value(GLib.Variant("()", ()))
                return
            if method == "PauseAuthenticationReads":
                pause_authentication = parameters.unpack()[0]
                if not pause_authentication:
                    for pending, value in authentication_reads:
                        pending.return_value(GLib.Variant("(b)", (value,)))
                    authentication_reads.clear()
                invocation.return_value(GLib.Variant("()", ()))
                return
            requested = parameters.unpack()[0]
            if requested not in ("stall", "error", "success", "delay"):
                invocation.return_dbus_error(
                    "org.freedesktop.DBus.Error.InvalidArgs", "Unknown fixture mode"
                )
                return
            mode = requested
            invocation.return_value(GLib.Variant("()", ()))
        elif method == "ListSyncFolders":
            rows = [
                ("selected-sync", "Saved selection", "opaque-folder-id",
                 "My Drive/Parent/Selected", "/fixture/selected", "", "", False, False),
                ("legacy-sync", "Legacy selection", "legacy-opaque-id",
                 "", "/fixture/legacy", "", "", False, False),
                ("root-sync", "Root selection", "", "", "/fixture/root", "", "", False, False),
            ]
            invocation.return_value(GLib.Variant("(a(sssssssbb))", (rows,)))
        elif method == "IsAuthenticated":
            if pause_authentication:
                if len(authentication_reads) >= 100:
                    invocation.return_dbus_error(
                        "org.freedesktop.DBus.Error.LimitsExceeded", "Too many pending auth reads"
                    )
                else:
                    authentication_reads.append((invocation, authenticated))
            else:
                invocation.return_value(GLib.Variant("(b)", (authenticated,)))
        elif method == "ListDriveFolders":
            parent = parameters.unpack()[0]
            rows = [("projects", "Projects")] if not parent or parent == "root" else []
            if mode == "stall":
                # Deliberately withhold a method reply to exercise the client's deadline.
                if len(stalled) < 100:
                    stalled.append(invocation)
                else:
                    invocation.return_dbus_error(
                        "org.freedesktop.DBus.Error.LimitsExceeded", "Too many stalled requests"
                    )
            elif mode == "error":
                invocation.return_dbus_error(
                    "org.freedesktop.DBus.Error.Timeout", "Fixture Drive request timed out"
                )
            elif mode == "delay":
                def complete():
                    invocation.return_value(GLib.Variant("(a(ss))", (rows,)))
                    return GLib.SOURCE_REMOVE
                GLib.timeout_add(1500, complete)
            else:
                invocation.return_value(GLib.Variant("(a(ss))", (rows,)))

    node = Gio.DBusNodeInfo.new_for_xml(XML)
    for interface in node.interfaces:
        path = {
            "org.freedesktop.portal.Settings": "/org/freedesktop/portal/desktop",
            "org.a11y.Bus": "/org/a11y/bus",
        }.get(interface.name, "/org/gclient/GDrive1")
        connection.register_object(path, interface, call, None, None)
    for name in ("org.freedesktop.portal.Desktop", "org.a11y.Bus", "org.gclient.GDrive1"):
        reply = connection.call_sync(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "RequestName",
            GLib.Variant("(su)", (name, 4)),
            GLib.VariantType.new("(u)"),
            Gio.DBusCallFlags.NONE,
            3000,
            None,
        )
        if reply.unpack()[0] != 1:
            sys.exit(f"Mock bus name {name} already owned; refusing to reuse another service")
    GLib.unix_signal_add(GLib.PRIORITY_DEFAULT, signal.SIGTERM, loop.quit)
    print("Mock daemon ready", flush=True)
    loop.run()


if __name__ == "__main__":
    main()
