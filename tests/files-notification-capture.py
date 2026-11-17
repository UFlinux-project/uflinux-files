#!/usr/bin/env python3
"""Capture desktop notification bodies on the smoke test's private session bus."""
import json
import sys
from pathlib import Path
from gi.repository import Gio, GLib

XML = '''<node><interface name="org.freedesktop.Notifications">
<method name="Notify"><arg type="s" direction="in"/><arg type="u" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="as" direction="in"/><arg type="a{sv}" direction="in"/><arg type="i" direction="in"/><arg type="u" direction="out"/></method>
<method name="GetCapabilities"><arg type="as" direction="out"/></method>
<method name="GetServerInformation"><arg type="s" direction="out"/><arg type="s" direction="out"/><arg type="s" direction="out"/><arg type="s" direction="out"/></method>
<method name="CloseNotification"><arg type="u" direction="in"/></method>
</interface></node>'''
log = Path(sys.argv[1])

def call(_connection, _sender, _path, _interface, method, parameters, invocation):
    if method == 'Notify':
        with log.open('a') as output:
            output.write(json.dumps(parameters.unpack()[4]) + '\n')
        reply = GLib.Variant('(u)', (1,))
    elif method == 'GetCapabilities':
        reply = GLib.Variant('(as)', (['body', 'actions'],))
    elif method == 'GetServerInformation':
        reply = GLib.Variant('(ssss)', ('Files test', 'UFLinux', '1', '1.2'))
    else:
        reply = None
    invocation.return_value(reply)

def acquired(connection, _name):
    connection.register_object('/org/freedesktop/Notifications', Gio.DBusNodeInfo.new_for_xml(XML).interfaces[0], call, None, None)
    log.touch()
    Path(str(log) + '.ready').touch()

Gio.bus_own_name(Gio.BusType.SESSION, 'org.freedesktop.Notifications', Gio.BusNameOwnerFlags.NONE, acquired, None, None)
GLib.MainLoop().run()
