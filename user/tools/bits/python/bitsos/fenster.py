"""Fenster: ``liste()``, ``vor(f)``, ``schliessen(f)``, ``groesse(f, breite, hoehe)`` oder ``groesse(f, voll=True)``.

Ein Fenster heißt wie in der Liste der Oberfläche: ``"w3"``."""

from ._bits import FalscherAufruf, bits


def liste():
    """Alle Fenster: Liste von dicts mit id, app, title, focused, place, rect, ..."""
    return bits("fenster", "liste")["windows"]


def vor(fenster):
    """Holt das Fenster nach vorn."""
    bits("fenster", "vor", fenster)


def schliessen(fenster):
    """Bittet das Fenster zu schließen."""
    bits("fenster", "schliessen", fenster)


def groesse(fenster, breite=None, hoehe=None, voll=False):
    """Macht das Fenster schwebend und gibt ihm die Größe (oder den ganzen Bildschirm); gibt [x, y, b, h] zurück."""
    if voll:
        args = ["voll"]
    elif breite is None or hoehe is None:
        raise FalscherAufruf("groesse braucht Breite und Höhe oder voll=True.", "usage")
    else:
        args = [breite, hoehe]
    return bits("fenster", "groesse", fenster, *args)["rect"]
