"""Dateien finden: ``suchen(woerter, anzahl=None, seit=None)`` im Verzeichnis des Kontextdienstes."""

from ._bits import bits


def suchen(woerter, anzahl=None, seit=None):
    """Treffer als Liste von dicts (path, title, snippet, ...)."""
    args = ["dateien", "suchen", woerter]
    if anzahl is not None:
        args += ["--anzahl", anzahl]
    if seit is not None:
        args += ["--seit", seit]
    r = bits(*args)["result"]
    return r.get("hits", r.get("results", []))
