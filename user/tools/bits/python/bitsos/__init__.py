"""BitsOS aus Python bedienen: eine dünne Schicht über ``bits --json`` (user/tools/bits/BITS.md ist der Vertrag).

Jedes Thema von ``bits`` ist hier ein Teil: ``bitsos.fenster``, ``bitsos.ui``, ``bitsos.einstellungen``,
``bitsos.arbeitsraum``, ``bitsos.dateien``, ``bitsos.zwischenablage``, ``bitsos.messwerte``,
``bitsos.programm``, ``bitsos.papierkorb``; ``bitsos.foto(...)`` und ``bitsos.meldung(...)`` sind Funktionen.
Was nicht geht, ist eine Ausnahme mit dem deutschen Satz von ``bits``.
"""

from ._bits import Abgelehnt, FalscherAufruf, Fehler, NichtErreichbar, ZeitAbgelaufen, bits
from . import arbeitsraum, dateien, einstellungen, fenster, messwerte, papierkorb, programm, ui, zwischenablage
from .foto import foto
from .meldung import meldung

__all__ = [
    "Abgelehnt", "FalscherAufruf", "Fehler", "NichtErreichbar", "ZeitAbgelaufen", "bits",
    "arbeitsraum", "dateien", "einstellungen", "fenster", "foto", "meldung", "messwerte", "papierkorb",
    "programm", "ui", "zwischenablage",
]
