"""Arbeitsräume des Kontextdienstes: ``liste``, ``oeffnen``, ``hinzufuegen``, ``umbenennen``, ``anheften``."""

from ._bits import bits


def liste(suche=None):
    """Die Arbeitsräume (Liste von dicts: id, name, pinned, items, last_used)."""
    return bits("arbeitsraum", "liste", *([suche] if suche else []))["result"]["workspaces"]


def oeffnen(name=None, nummer=None):
    """Öffnet einen Arbeitsraum nach Name oder Nummer; gibt ihn zurück."""
    return bits("arbeitsraum", "oeffnen", *(["--nummer", nummer] if nummer else [name]))["result"]


def hinzufuegen(nummer, datei):
    return bits("arbeitsraum", "hinzufuegen", nummer, datei)["result"]


def umbenennen(nummer, name):
    return bits("arbeitsraum", "umbenennen", nummer, name)["result"]


def anheften(nummer, an=True):
    return bits("arbeitsraum", "anheften", nummer, *([] if an else ["--los"]))["result"]
