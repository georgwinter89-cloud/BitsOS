"""Die Zwischenablage: ``lesen()`` und ``setzen(text)``."""

from ._bits import bits


def lesen():
    """Der Text in der Zwischenablage."""
    return bits("zwischenablage", "lesen")["text"]


def setzen(text):
    """Legt den Text in die Zwischenablage (höchstens 1 MiB)."""
    bits("zwischenablage", "setzen", "-", eingabe=text)
