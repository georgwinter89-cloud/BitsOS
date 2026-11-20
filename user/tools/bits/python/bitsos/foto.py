"""``bitsos.foto(datei, fenster=None)``: ein Bild des Bildschirms oder eines Fensters (PNG)."""

from ._bits import bits


def foto(datei, fenster=None):
    """Schreibt das Bild nach ``datei``; gibt ``{"file", "width", "height"}`` zurück."""
    return bits("foto", *(["--fenster", fenster] if fenster else []), datei)
