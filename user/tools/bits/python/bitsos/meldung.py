"""``bitsos.meldung(text)``: eine Zeile in der Meldungsleiste der Oberfläche."""

from ._bits import bits


def meldung(text):
    """Zeigt die Zeile; gibt zurück, was angezeigt wird."""
    return bits("meldung", text)["shown"]
