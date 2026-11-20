# bits: ein Befehl für BitsOS

`bits` bedient BitsOS vom Terminal und aus Skripten: Fenster, die Oberfläche als Liste, Einstellungen,
Arbeitsräume, Dateien, Zwischenablage, Bilder, Meldungen, Messwerte, Programme und den Papierkorb.

    bits [--json] <thema> <verb> [...]
    bits hilfe [thema]

Jedes Thema hat genau eine Schreibweise (deutsche Wörter ohne Umlaute: `groesse`, `zurueck`, `oeffnen`).
Mit `--json` (an beliebiger Stelle) schreibt jeder Aufruf genau ein JSON-Objekt auf die Ausgabe:
`{"ok":true, ...}` oder `{"ok":false,"error":"<art>","message":"<deutscher Satz>"}` (wie der Einstellungs-
und der Programmdienst). Ohne `--json` kommen Zeilen für Menschen; Fehler stehen dann auf der Fehlerausgabe.

Rückgabewerte: 0 erledigt; 1 ging nicht (`failed`, `not_found`); 2 Aufruf falsch (`usage`); 3 ein Dienst
antwortet nicht (`unreachable`); 4 Zeit abgelaufen (`timeout`); 5 nicht erlaubt: die Stufe, eine feste Regel,
Georg hat Nein gesagt oder nicht geantwortet (`refused`).

Aus dem Assistenten heraus (seinem Prozessbaum) gilt für `bits` dasselbe wie für die eigenen Werkzeuge des
Assistenten: Lesen geht immer; Ändern ist bei „Nur antworten“ abgelehnt, bei „Fragt vorher“ fragt die Oberfläche
Georg, im Automodus läuft es. Die festen Regeln gelten immer: keine Passwörter, Schlüssel oder Bezahldaten
eintippen; vor Kaufen, Bezahlen, Senden, Veröffentlichen fragen; geschützte Einstellungen setzt nur Georg;
den Papierkorb leert nur Georg. Aus Georgs eigenem Terminal geht alles wie bisher.

Python: das Modul `bitsos` (siehe `bits hilfe python`) ruft `bits --json` und hat dieselben Verben.

`bits hilfe` gibt `{"topic":"","text":"…","topics":["fenster",…]}`, `bits hilfe <thema>` gibt
`{"topic":"ui","text":"## ui …"}` (der Abschnitt dieses Textes).

## fenster

    bits fenster liste
    bits fenster vor <fenster>
    bits fenster schliessen <fenster>
    bits fenster groesse <fenster> <breite> <höhe> | voll

Ein Fenster heißt wie in der Liste der Oberfläche: `w3` (die Zahl allein geht auch). `liste` gibt
`{"windows":[{"id":"w3","app":"foot","title":"…","pid":123,"focused":true,"visible":true,"place":"tile|float|minimized",
"fullscreen":false,"max":false,"hung":false,"workspace":1,"rect":[x,y,breite,höhe]}]}` (`rect` ist bei minimierten
Fenstern `null`). `vor` holt das Fenster nach vorn (auch von einem anderen Arbeitsbereich oder aus dem Minimierten),
`schliessen` bittet es zu schließen, `groesse` macht ein Fenster schwebend und gibt ihm die Größe (oder `voll`:
ganzer Bildschirm); Antwort `{"window":"w3","rect":[...]}`. Ändernde Verben: siehe „Fragen“.

    bits fenster liste
    bits --json fenster vor w3

## ui

    bits ui liste [fenster]
    bits ui tun <element> <aktion> [text] [--rueckgaengig | --endgueltig]
    bits ui warten [--fenster F] [--element E] [--text T] [--weg] [--bis-zu S]

`liste` zeigt die Oberfläche als Liste (Vertrag C1): ohne Fenster alle Fenster, die Elemente nur vom Fenster
vorn; mit Fenster (`w3`, `focus`, `desktop`) dieses mit allen Elementen. JSON: `{"windows":[…]}` bzw.
`{"window":{"window":"w3","app":…,"title":…,"pid":…,"source":…,"focused":…,"rect":[…],"elements":[…]}}`, ein Element
`{"id":"w3/1a2b","role":…,"name":…,"value"?,"state"?,"rect":[…],"actions"?,"children"?}`. Werte geheimer Felder fehlen
immer.

`tun` macht einen Schritt wie eine echte Eingabe. Aktionen: `press`, `type`, `choose`, `toggle`, `scroll`,
`focus`, `submit` (Enter in genau diesem Feld). `type` ersetzt den Inhalt des Feldes; ein `\n` am Ende ist Enter.
Antwort `{"element":"w3/1a2b","action":"type","answer":"ok"}`. Aus dem Assistenten heraus sagt `--rueckgaengig`,
dass sich der Schritt rückgängig machen lässt (`--endgueltig`: dass nicht), wie das Modell es in Durchblick sagt;
ohne dieses Wort fragt die Oberfläche bei einem Drücken auf jeder Stufe, weil nicht sicher ist, was es tut. Was
kauft, bezahlt, sendet, veröffentlicht, löscht oder Bedingungen zustimmt, wird immer gefragt.

`warten` wartet, bis etwas da ist (mit `--weg`: bis es weg ist), und zählt erst, wenn zwei Blicke im Abstand von
0,3 s dasselbe an derselben Stelle zeigen: eine Seite, die noch lädt, ist nicht fertig. `--fenster` ist eine
Fensternummer oder ein Wort aus Titel oder Programmname, `--element` eine Elementnummer, `--text` ein Wort aus Name
oder Wert eines Elements (ohne `--fenster` im Fenster vorn). `--bis-zu` Sekunden (Vorgabe 10). Antwort
`{"window":{"id","app","title"},"element":{…}|null,"waited_ms":n}`; abgelaufen: Rückgabewert 4.

    bits ui warten --fenster Brave --text "Anmelden" --bis-zu 20
    bits ui tun w3/1a2b type "Hallo Welt"
    bits ui tun w3/9f0c press

## einstellungen

    bits einstellungen liste [anfang]
    bits einstellungen lesen <schlüssel>
    bits einstellungen setzen <schlüssel> <wert> [--bestaetigt]

Über den Einstellungsdienst mit seinen Regeln (Typen, Grenzen, gesperrte Schlüssel, geheime Werte als `***`).
`liste`: `{"keys":[{"key","type","value","changed","text"}]}`; `lesen`: `{"key","value","changed"}`; `setzen`:
`{"key","old","value"}`. `--bestaetigt` sagt, dass Georg zugestimmt hat (wie `settings set --confirm`); aus dem
Assistenten heraus zählt es nie, und eine Einstellung, die vorher fragt, bleibt Georgs.

    bits einstellungen lesen ai.level

## arbeitsraum

    bits arbeitsraum liste [suche]
    bits arbeitsraum oeffnen <name> | --nummer <id>
    bits arbeitsraum hinzufuegen <id> <datei>
    bits arbeitsraum umbenennen <id> <neuer name>
    bits arbeitsraum anheften <id> [--los]

Die Arbeitsräume des Kontextdienstes. Die Antwort steht unter `result`, so wie der Kontextdienst sie gibt
(`liste`: `{"result":{"workspaces":[{"id","name","pinned","items","last_used"}]}}`). Aus dem Assistenten heraus sieht
`bits` dasselbe wie die Werkzeuge des Assistenten (keine privaten Ordner).

## dateien

    bits dateien suchen <wörter> [--anzahl n] [--seit JJJJ-MM-TT]

Sucht im Verzeichnis des Kontextdienstes. `{"result":{"hits":[{"path","title","snippet",…}],…}}`.

## zwischenablage

    bits zwischenablage lesen
    bits zwischenablage setzen <text> | -

`lesen`: `{"text":"…"}` (ohne `--json` der Text selbst). `setzen` nimmt den Text aus den Argumenten oder (ohne
Text oder mit `-`) von der Eingabe, höchstens 1 MiB: `{"chars":n}`. Setzen ist ein ändernder Schritt.

    ls | bits zwischenablage setzen -

## foto

    bits foto [--fenster F] <datei>

Ein Bild des Bildschirms oder eines Fensters, als PNG (`.ppm`: das Rohbild). `{"file","width","height"}`. Fenster
von Programmen auf der Liste „bei diesen Programmen nie“ fotografiert der Assistent nicht.

## meldung

    bits meldung <text>

Eine Zeile in der Meldungsleiste der Oberfläche (`{"shown":"…"}`); aus dem Assistenten heraus mit „Assistent:“
davor.

## messwerte

    bits messwerte [liste]
    bits messwerte lesen <name>...

Die Messwerte des Kerns (`/proc/bitsos/metrics`): `{"values":[{"name","value","unit"}]}`. Ein `*` am Ende eines
Namens nimmt alle, die so beginnen (`cpu.*`).

## programm

    bits programm start <name>
    bits programm finde <name>
    bits programm satz [skripte] [--installieren]
    bits programm befehle

`start` startet ein Programm der Bibliothek (`{"answer":"…"}`), `finde` sagt, welcher Eintrag gemeint ist
(`{"id","name"}`). `satz` zeigt den Satz „skripte“ (Python, git, curl, jq, ripgrep: je ein Programm aus den
Paketquellen mit allem, was es braucht) und was das Installieren lädt; `--installieren` installiert die fehlenden
(das darf root an der Konsole). `befehle` zeigt die Befehle des Satzes (`python3`, `python`, `git`, `curl`, `jq`,
`rg`) und ob ihr Programm installiert ist.

`satz`: `{"set":"skripte","download":n,"programs":[…]}`, `download` in Bytes (gemeinsame Pakete einmal gezählt).
Ein installiertes Programm ist `{"program","commands":[…],"installed":"<version>"}`, ein fehlendes
`{"program","commands","installed":null,"installable":true|false,"download":n,"summary":"…"}`; mit `--installieren`
steht danach bei `installed` der Name des installierten Programms und unter `report` der Bericht des
Programmdienstes (Liste von Zeilen). `befehle`: `{"commands":[{"program":"python","commands":["python3","python"],
"installed":"<version>"|null}]}`. Diese Befehle sehen Georgs Dateien, /tmp und die Dienste von BitsOS;
`bits` und das Python-Modul `bitsos` sind darin da.

## papierkorb

    bits papierkorb liste
    bits papierkorb zurueck <id> [--nach <pfad>]
    bits papierkorb leeren [<id>]

Was der Assistent löscht, landet im Papierkorb seines Datenträgers (`/data/.papierkorb`, auf dem Stick
`/boot/BITSOS2/home/.papierkorb`; ein Eintrag je Befehl, `rm -r` ist einer). `mount` ist der Datenträger, `original`
der Ort des Gelöschten (bei einem Ordner der Ordner). Zurücklegen und Leeren macht der Kern
(`/dev/bitsos/papierkorb`), der prüft, ob der Aufrufer dort etwas anlegen dürfte. `liste`: `{"entries":[{"id","mount","original",
"time","command","chat","pid","kind":"file|folder","bytes"}],"bytes":n}`. `zurueck` legt einen Eintrag an seinen Ort
zurück (nie über etwas, das jetzt dort liegt; `--nach` nimmt einen anderen Ort): `{"id":"…","restored":"<pfad>"}`.
`leeren` löscht endgültig, alles oder einen Eintrag; die Oberfläche fragt Georg vorher, und aus dem Assistenten
heraus geht es nie: `{"removed":<anzahl einträge>,"bytes":n}`.

## fragen

Ein ändernder Schritt aus dem Assistenten heraus bei „Fragt vorher“ (und ein Schritt, der etwas kauft, sendet
oder nicht rückgängig zu machen ist, auf jeder Stufe) wird auf der Oberfläche gefragt. `bits` wartet auf die
Antwort, höchstens so lange, wie die Frage offen ist (120 s), oder so lange, wie `BITS_FRAGE_S` sagt. Nur Georgs
echte Maus oder Tastatur sagt Ja. Ohne Ja: Rückgabewert 5, es wurde nichts getan.

## python

    import bitsos
    for f in bitsos.fenster.liste():
        print(f["id"], f["title"])
    bitsos.ui.warte(text="Anmelden", bis_zu=20)
    bitsos.ui.tun("w3/1a2b", "type", "Hallo")

Jedes Thema mit mehreren Verben ist ein Teil des Moduls (`bitsos.fenster`, `bitsos.ui`, `bitsos.einstellungen`,
`bitsos.arbeitsraum`, `bitsos.dateien`, `bitsos.zwischenablage`, `bitsos.messwerte`, `bitsos.programm`,
`bitsos.papierkorb`: `bitsos.<thema>.<verb>(…)`). Die zwei Themen mit nur einem Verb sind Funktionen:
`bitsos.foto(datei, fenster=None)` und `bitsos.meldung(text)`. Die Schalter heißen wie Argumente:
`bitsos.ui.tun(e, "press", rueckgaengig=True)`, `bitsos.einstellungen.setzen(k, w, bestaetigt=True)`,
`bitsos.programm.satz(installieren=True)`. Ein Fehler ist eine Ausnahme `bitsos.Fehler` (bzw.
`Abgelehnt`, `ZeitAbgelaufen`, `NichtErreichbar`) mit dem deutschen Satz; `.art` ist die Art aus dem JSON. Beispiele
liegen in `user/tools/bits/python/beispiele/`.

## umgebung

Für Tests und besondere Fälle: `BITS_CTL` (der Steuer-Anschluss der Oberfläche), `SETTINGS_SOCKET`,
`BITS_CTXD`, `BITS_PKG`, `BITS_METRICS`, `BITS_PAPIERKORB_MOUNTS` (Einhängepunkte, mit `:` getrennt), `BITS_APPS`
(Wurzel der installierten Programme), `BITS_BWRAP`, `BITS_FRAGE_S`. Das Python-Modul nimmt `BITS` als Pfad zu `bits`.
