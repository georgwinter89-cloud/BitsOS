"""Der Papierkorb: ``liste()``, ``zurueck(id, nach=None)``, ``leeren(id=None)`` (fragt Georg; nie aus dem Assistenten)."""

from ._bits import bits


def liste():
    """Die Einträge (id, original, time, command, kind, bytes, ...), die neuesten zuerst."""
    return bits("papierkorb", "liste")["entries"]


def zurueck(id, nach=None):
    """Legt einen Eintrag an seinen Ort zurück (oder nach ``nach``); gibt den Pfad zurück."""
    return bits("papierkorb", "zurueck", id, *(["--nach", nach] if nach else []))["restored"]


def leeren(id=None):
    """Löscht endgültig, nach Georgs Ja auf der Oberfläche; gibt die Zahl der Einträge zurück."""
    return bits("papierkorb", "leeren", *([id] if id else []))["removed"]
