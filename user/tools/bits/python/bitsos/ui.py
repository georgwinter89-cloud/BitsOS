"""Die Oberfläche als Liste: ``liste(fenster)``, ``tun(element, aktion, text)``, ``warte(...)``.

Aktionen: press, type, choose, toggle, scroll, focus, submit. ``type`` ersetzt den Inhalt eines Feldes."""

from ._bits import bits

AKTIONEN = ("press", "type", "choose", "toggle", "scroll", "focus", "submit")


def liste(fenster=None):
    """Ohne Fenster: alle Fenster (Elemente nur vom Fenster vorn). Mit Fenster (``"w3"``, ``"focus"``): dieses."""
    if fenster is None:
        return bits("ui", "liste")["windows"]
    return bits("ui", "liste", fenster)["window"]


def tun(element, aktion, text=None, rueckgaengig=None):
    """Ein Schritt wie eine echte Eingabe; gibt die Antwort der Oberfläche zurück.

    ``rueckgaengig=True`` sagt, dass sich der Schritt rückgängig machen lässt (``False``: dass nicht); ohne fragt
    die Oberfläche beim Drücken aus dem Assistenten heraus auf jeder Stufe."""
    args = ["ui", "tun", element, aktion] + ([text] if text is not None else [])
    if rueckgaengig is not None:
        args.append("--rueckgaengig" if rueckgaengig else "--endgueltig")
    return bits(*args)["answer"]


def tippe(element, text):
    """Kurz für ``tun(element, "type", text)``."""
    return tun(element, "type", text)


def druecke(element, rueckgaengig=None):
    """Kurz für ``tun(element, "press")``."""
    return tun(element, "press", rueckgaengig=rueckgaengig)


def warte(fenster=None, element=None, text=None, weg=False, bis_zu=10):
    """Wartet, bis ein Fenster, ein Element oder ein Text da ist (``weg=True``: weg), wenn die Seite fertig ist.

    Gibt ``{"window": ..., "element": ..., "waited_ms": n}`` zurück; nach ``bis_zu`` Sekunden ``ZeitAbgelaufen``."""
    args = ["ui", "warten", "--bis-zu", bis_zu]
    for flag, wert in (("--fenster", fenster), ("--element", element), ("--text", text)):
        if wert is not None:
            args += [flag, wert]
    if weg:
        args.append("--weg")
    return bits(*args)


def finde(fenster, name=None, rolle=None):
    """Das erste Element eines Fensters mit diesem Namen (und dieser Rolle), oder None."""
    def gang(els):
        for e in els:
            if (name is None or e.get("name") == name) and (rolle is None or e.get("role") == rolle):
                return e
            hit = gang(e.get("children", []))
            if hit:
                return hit
        return None
    return gang(liste(fenster).get("elements", []))
