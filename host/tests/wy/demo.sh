#!/usr/bin/env bash
# WY-0c/WY-5 demo for Georg: bitscomp as a window on the KDE desktop, with foot and Brave inside.
# Close it with Strg+Alt+Q or the window's close button; the script then ends Brave and the test page.
# Not a fullrun scenario (it opens a window), so it does nothing when the test runner calls it.
set -euo pipefail

if [ -n "${BITSOS_RUN_DIR:-}" ]; then
    echo "demo.sh opens a window; skipped in the test run"
    exit 0
fi

ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=$CARGO_TARGET_DIR/release/bitscomp
SOCK=wayland-bits-demo
SIZE=${1:-1800x1100}
PORT=30500

(cd "$ROOT/user/services/bitscomp" && cargo build --release -q)

PIDS=()
cleanup() {
    for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
}
trap cleanup EXIT

python3 -m http.server "$PORT" --bind 127.0.0.1 --directory "$ROOT/host/tests/wy/page" >/dev/null 2>&1 &
PIDS+=($!)

cat <<'EOF'
bitscomp (WY-5, das Aussehen nach dem Entwurf) - alles geht mit der Maus, die Tasten (alle mit Strg+Alt) sind
Abkürzungen:

  Oben: Adresszeile. Klick hinein (oder Strg+Alt+L) und tippen: Vorschläge erscheinen (Favoriten, Programme,
    Einstellungen, zuletzt benutzt). Enter öffnet: "programm/terminal" (auch "terminal") startet ein Terminal
    als neuen Reiter, Strg+Enter in einer neuen Kachel; "einstellungen/aussehen" zeigt die Design-Einstellungen;
    Webseiten gehen noch nicht (ein Hinweis sagt das). Esc schließt die Zeile.
  Darunter die Favoritenleiste: Klick öffnet. Rechts oben der Ring: klappt Claudes Seitenleiste ein und aus
    (Strg+Alt+C).
  Links die Arbeitsräume: Klick wechselt, "+ Neuer Arbeitsraum" legt einen an      Strg+Alt+1..9, Strg+Alt+N
    Der Knopf oben in der Liste klappt sie ganz weg (Strg+Alt+W; Fenster schließen ist jetzt Strg+Alt+X);
    zurück mit dem Knopf links oben, Strg+Alt+W,
    oder Mauszeiger 0,3 s an den linken Rand drücken: dann liegt sie über den Kacheln, bis der Zeiger sie
    verlässt (ihr Knopf heftet sie fest). Claudes Leiste genauso am rechten Rand.
    Ein leerer Arbeitsraum zeigt die Startseite (Letzte Sitzung wiederherstellen, Neue Sitzung, Favoriten,
    zuletzt benutzt, Bausteine); "Startseite" unten links zeigt sie auch.
  Unten die Statusleiste: CPU, Netz (und Temperatur, wo es sie gibt), die Uhr.

  Fenster und Reiter                                    Taste
  Klick in ein Fenster oder auf seine Kopfleiste        Pfeil links/rechts   Fokus wechseln
  Reiter (wie im Browser): Klick zeigt dieses Fenster   Bild ab/auf          blättern
  Mausrad auf der Kopfleiste blättert auch; "+N" zeigt den Rest
  [x] auf einem Reiter: genau dieses Fenster schließen
  Reiter ziehen: in der Leiste = Reihenfolge; auf eine Kachel = auf ihren Stapel;
    an ihren Rand = daneben; in eine Lücke = eigene Kachel                 S (Stapel), U (aus dem Stapel)
  Knopf [Schweben] rechts auf der Kopfleiste            F
  Schwebende Fenster liegen über allem und lassen sich über den ganzen Bildschirm ziehen;
    Doppelklick auf ihre Kopfleiste oder Knopf [Maximieren]: ganzer Bildschirm und zurück   M, Super+Pfeil hoch
  Knopf [x] rechts auf der Kopfleiste: schließen        X
  Lücke zwischen zwei Kacheln ziehen                    Plus/Minus           breiter/schmaler
  Rechtsklick auf eine Kopfleiste: Kreismenü; auf freie Fläche: Kreismenü mit Neues Terminal (= Enter),
    Neuer Arbeitsraum, Arbeitsraum entfernen (nur leer), bitscomp beenden (= Q)
  Hängt ein Programm: "antwortet nicht" mit "Beenden" und "Warten" (blendet den Schleier 10 s aus).

Aussehen ändern (hell/dunkel, Akzentfarbe, Leisten an/aus) geht über die Einstellungen (settingsd), auf V2 auch
über Claude ("settings_set theme.mode light"); ohne settingsd gilt das Aussehen des Entwurfs.
Beenden: Strg+Alt+Q, Kreismenü "bitscomp beenden", Fenster schließen oder Strg+C hier im Terminal.
EOF

"$BIN" --backend winit --size "$SIZE" --socket "$SOCK" --run "foot" &
COMP=$!
PIDS+=($COMP)
for _ in $(seq 50); do [ -S "${XDG_RUNTIME_DIR:-/tmp}/$SOCK" ] && break; sleep 0.1; done

mkdir -p /tmp/bitscomp-demo-brave
WAYLAND_DISPLAY=$SOCK brave --ozone-platform=wayland --disable-gpu --password-store=basic \
    --user-data-dir=/tmp/bitscomp-demo-brave --no-first-run --no-default-browser-check --disable-breakpad \
    "http://127.0.0.1:$PORT/" "https://example.com" >/dev/null 2>&1 &
PIDS+=($!)

wait "$COMP" || true
