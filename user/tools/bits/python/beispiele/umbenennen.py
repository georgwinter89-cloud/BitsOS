#!/usr/bin/env python3
"""Dateien eines Ordners nach einem Muster umbenennen.

    python3 umbenennen.py <ordner> <muster> <ersatz> [--probe]

<muster> ist ein regulärer Ausdruck für den Dateinamen, <ersatz> der neue Name (\\1 für die erste Klammer).
Beispiel: python3 umbenennen.py ~/Bilder 'IMG_(\\d+)\\.jpg' 'Urlaub-\\1.jpg'
Mit --probe zeigt es nur, was es täte. Eine Datei, deren neuer Name schon da ist, bleibt, wie sie ist.
Am Ende sagt eine Meldung auf der Oberfläche, wie viele es waren."""

import os
import re
import sys

import bitsos


def main(args):
    probe = "--probe" in args
    args = [a for a in args if a != "--probe"]
    if len(args) != 3:
        print(__doc__)
        return 2
    ordner, muster, ersatz = args
    regel = re.compile(muster)
    fertig = 0
    for name in sorted(os.listdir(ordner)):
        if not regel.fullmatch(name):
            continue
        neu = regel.sub(ersatz, name)
        alt_pfad, neu_pfad = os.path.join(ordner, name), os.path.join(ordner, neu)
        if neu == name or os.path.exists(neu_pfad):
            print(f"bleibt: {name}")
            continue
        print(f"{name} -> {neu}")
        if not probe:
            os.rename(alt_pfad, neu_pfad)
            fertig += 1
    if not probe:
        bitsos.meldung(f"{fertig} Dateien umbenannt")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
