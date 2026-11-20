"""Ein Aufruf von ``bits --json`` und seine Antwort; Fehler als Ausnahmen."""

import json
import os
import shutil
import subprocess


class Fehler(Exception):
    """Etwas ging nicht. ``meldung`` ist der deutsche Satz, ``art`` die Art aus dem JSON (``failed``, ...)."""

    def __init__(self, meldung, art="failed"):
        super().__init__(meldung)
        self.meldung = meldung
        self.art = art


class Abgelehnt(Fehler):
    """Nicht erlaubt: die Stufe, eine feste Regel, oder Georg hat Nein gesagt oder nicht geantwortet."""


class ZeitAbgelaufen(Fehler):
    """Worauf gewartet wurde, kam nicht rechtzeitig."""


class NichtErreichbar(Fehler):
    """Ein Dienst von BitsOS antwortet nicht."""


class FalscherAufruf(Fehler):
    """Der Aufruf war falsch (unbekanntes Verb, fehlendes Argument)."""


_ARTEN = {"refused": Abgelehnt, "timeout": ZeitAbgelaufen, "unreachable": NichtErreichbar, "usage": FalscherAufruf}


def _programm():
    for p in (os.environ.get("BITS"), shutil.which("bits"), "/run/bitsos-system/bin/bits", "/bin/bits"):
        if p and os.path.exists(p):
            return p
    raise NichtErreichbar("Den Befehl bits gibt es hier nicht.", "unreachable")


def bits(*args, eingabe=None):
    """Ruft ``bits --json <args>`` und gibt das JSON-Objekt der Antwort (ohne ``ok``) zurück."""
    r = subprocess.run([_programm(), "--json", *[str(a) for a in args]], input=eingabe, capture_output=True, text=True)
    try:
        antwort = json.loads(r.stdout)
    except ValueError:
        raise Fehler(f"bits antwortet unverständlich: {(r.stdout or r.stderr).strip()[:200]}")
    if not antwort.pop("ok", False):
        art = antwort.get("error", "failed")
        raise _ARTEN.get(art, Fehler)(antwort.get("message", "abgelehnt"), art)
    return antwort
