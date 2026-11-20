"""Messwerte des Kerns: ``liste()`` und ``lesen(*namen)`` (ein ``*`` am Ende nimmt alle, die so beginnen)."""

from ._bits import bits


def liste():
    """Alle Messwerte als Liste von dicts: name, value, unit."""
    return bits("messwerte", "liste")["values"]


def lesen(*namen):
    """Die genannten Messwerte als dict name -> Wert."""
    return {v["name"]: v["value"] for v in bits("messwerte", "lesen", *namen)["values"]}
