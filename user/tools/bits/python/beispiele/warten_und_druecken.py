#!/usr/bin/env python3
"""Auf ein Fenster warten und dann einen Knopf darin drücken.

    python3 warten_und_druecken.py <fenster> <knopf> [--rueckgaengig] [--bis-zu S]

Wartet (höchstens S Sekunden, Vorgabe 60), bis ein Fenster mit <fenster> im Titel oder Programmnamen da ist und
darin ein Knopf <knopf> zu sehen ist, und drückt ihn. --rueckgaengig sagt der Oberfläche, dass sich das Drücken
rückgängig machen lässt (sonst fragt sie aus dem Assistenten heraus vorher)."""

import sys

import bitsos


def main(args):
    undo = "--rueckgaengig" in args
    args = [a for a in args if a != "--rueckgaengig"]
    bis_zu = 60
    if "--bis-zu" in args:
        i = args.index("--bis-zu")
        bis_zu = float(args[i + 1])
        del args[i:i + 2]
    if len(args) != 2:
        print(__doc__)
        return 2
    fenster, knopf = args
    w = bitsos.ui.warte(fenster=fenster, bis_zu=bis_zu)["window"]["id"]
    bitsos.ui.warte(fenster=w, text=knopf, bis_zu=bis_zu)
    e = bitsos.ui.finde(w, name=knopf, rolle="button")
    if not e:
        print(f"Einen Knopf „{knopf}“ gibt es in {w} nicht.")
        return 1
    bitsos.ui.druecke(e["id"], rueckgaengig=True if undo else None)
    print(f"„{knopf}“ gedrückt.")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except bitsos.Fehler as f:
        print(f.meldung)
        sys.exit(1)
