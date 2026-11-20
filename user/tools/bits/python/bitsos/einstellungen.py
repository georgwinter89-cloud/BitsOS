"""Einstellungen über den Einstellungsdienst: ``liste(anfang)``, ``lesen(schluessel)``, ``setzen(schluessel, wert, bestaetigt=False)``."""

from ._bits import bits


def liste(anfang=""):
    """Alle Schlüssel (die mit ``anfang`` beginnen) als Liste von dicts: key, type, value, changed, text."""
    return bits("einstellungen", "liste", anfang)["keys"] if anfang else bits("einstellungen", "liste")["keys"]


def lesen(schluessel):
    """Der Wert eines Schlüssels (geheime Werte als ``***``)."""
    return bits("einstellungen", "lesen", schluessel)["value"]


def setzen(schluessel, wert, bestaetigt=False):
    """Setzt einen Wert; gibt den neuen Wert zurück. ``bestaetigt=True`` sagt, dass Georg zugestimmt hat (wie
    ``bits einstellungen setzen ... --bestaetigt``); aus dem Assistenten heraus zählt es nie."""
    if isinstance(wert, bool):
        wert = "true" if wert else "false"
    return bits("einstellungen", "setzen", schluessel, wert, *(["--bestaetigt"] if bestaetigt else []))["value"]
