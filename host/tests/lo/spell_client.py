#!/usr/bin/env python3
"""LO-1 (05-stage.sh): asks a running LibreOffice (soffice --accept=pipe,name=NAME;urp;) through UNO whether its
spell checker knows de-DE and judges words by the stage's dictionary. Uses the host's own Python UNO bridge
(libreoffice-fresh's uno.py); the staged tree has no Python. Usage: spell_client.py NAME GOOD BAD. Exit 0 = green."""
import sys
import time

import uno  # noqa: E402 (the host's LibreOffice package)
from com.sun.star.lang import Locale  # noqa: E402

name, good, bad = sys.argv[1], sys.argv[2], sys.argv[3]
local = uno.getComponentContext()
resolver = local.ServiceManager.createInstanceWithContext("com.sun.star.bridge.UnoUrlResolver", local)
ctx = None
for _ in range(120):
    try:
        ctx = resolver.resolve(f"uno:pipe,name={name};urp;StarOffice.ComponentContext")
        break
    except Exception:  # noqa: BLE001 (NoConnectException while LibreOffice starts)
        time.sleep(0.5)
if ctx is None:
    sys.exit(f"RED: no LibreOffice answers on the pipe {name}")
smgr = ctx.ServiceManager
lsm = smgr.createInstanceWithContext("com.sun.star.linguistic2.LinguServiceManager", ctx)
loc = Locale("de", "DE", "")
fails = []
# The spell checkers LibreOffice offers for de-DE: the hunspell one only when a de-DE dictionary is configured.
avail = lsm.getAvailableServices("com.sun.star.linguistic2.SpellChecker", loc)
if "org.openoffice.lingu.MySpellSpellChecker" not in avail:
    fails.append(f"no hunspell spell checker for de-DE (offered: {avail})")
else:
    sc = smgr.createInstanceWithContext("org.openoffice.lingu.MySpellSpellChecker", ctx)
    # Typed arguments: pyuno cannot guess the type of an empty property sequence.
    args = lambda w: (w, uno.Any("com.sun.star.lang.Locale", loc), uno.Any("[]com.sun.star.beans.PropertyValue", ()))  # noqa: E731
    if not sc.hasLocale(loc):
        fails.append("the hunspell spell checker has no de-DE")
    elif not uno.invoke(sc, "isValid", args(good)):
        fails.append(f"'{good}' (in the dictionary) is called wrong")
    elif uno.invoke(sc, "isValid", args(bad)):
        fails.append(f"'{bad}' (not in the dictionary) is called right")
desktop = smgr.createInstanceWithContext("com.sun.star.frame.Desktop", ctx)
try:
    desktop.terminate()
except Exception:  # noqa: BLE001 (the bridge goes away with LibreOffice)
    pass
if fails:
    sys.exit("RED: " + "; ".join(fails))
print(f"ok: de-DE spell checking from the stage's dictionary ('{good}' right, '{bad}' wrong)")
