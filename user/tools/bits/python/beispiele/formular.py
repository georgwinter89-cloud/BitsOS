#!/usr/bin/env python3
"""Ein Feld eines Webformulars ausfüllen und abschicken.

    python3 formular.py <fenster> <feld> <text>

<fenster> ist ein Wort aus Titel oder Programmname (``Brave``), <feld> der Name des Feldes, wie die Oberfläche
ihn nennt (``bits ui liste`` zeigt ihn). Das Skript wartet, bis Fenster und Feld da sind, tippt den Text und
drückt im Feld Enter. Aus dem Assistenten heraus fragt die Oberfläche beim Abschicken immer vorher."""

import sys

import bitsos


def main(args):
    if len(args) != 3:
        print(__doc__)
        return 2
    fenster, feld, text = args
    w = bitsos.ui.warte(fenster=fenster, text=feld, bis_zu=30)["window"]["id"]
    e = bitsos.ui.finde(w, name=feld, rolle="field") or bitsos.ui.finde(w, name=feld, rolle="choice")
    if not e:
        print(f"Ein Feld „{feld}“ hat das Fenster nicht.")
        return 1
    bitsos.ui.tun(e["id"], "type", text)
    if "submit" in e.get("actions", []):
        bitsos.ui.tun(e["id"], "submit")
    else:
        bitsos.ui.tun(e["id"], "type", text + "\n")
    print(f"„{text}“ in „{feld}“ abgeschickt.")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except bitsos.Fehler as f:
        print(f.meldung)
        sys.exit(1)
