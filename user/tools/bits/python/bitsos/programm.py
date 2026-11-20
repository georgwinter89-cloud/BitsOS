"""Programme: ``start(name)``, ``finde(name)``, ``satz(installieren=False)`` (der Satz „skripte“), ``befehle()``."""

from ._bits import bits


def start(name):
    """Startet ein Programm der Bibliothek; gibt die Antwort der Oberfläche zurück."""
    return bits("programm", "start", name)["answer"]


def finde(name):
    """Welcher Eintrag der Bibliothek gemeint ist: dict id, name."""
    r = bits("programm", "finde", name)
    return {"id": r["id"], "name": r["name"]}


def satz(installieren=False):
    """Der Satz „skripte“: je Programm installiert oder was es lädt. ``installieren=True`` installiert die
    fehlenden (das darf root an der Konsole, nie der Assistent)."""
    return bits("programm", "satz", *(["--installieren"] if installieren else []))["programs"]


def befehle():
    """Die Befehle des Satzes und ob ihr Programm installiert ist."""
    return bits("programm", "befehle")["commands"]
