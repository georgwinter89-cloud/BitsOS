# Bau-Läufer mit dem API-Guthaben (für Georg)

Dein Max-Plan gibt dir jeden Monat 200 $ API-Guthaben. Claude Code (diese App) darf das nicht nutzen, das
Agent SDK schon. Dieser Läufer baut die offenen A1-Punkte mit genau denselben Aufträgen wie der Workflow
der App weiter: je Punkt ein Bauer (Opus), ein Prüfer, ein Nachbesserer (Opus). Was schon fertig ist, überspringt
er. Er hört auf, bevor 190 $ verbraucht sind. Alles bleibt auf den Bahn-Zweigen. In den Hauptstand übernimmt
erst die nächste Claude-Sitzung.

**Wichtig:** Die Bau-Agenten des Läufers führen Befehle aus, ohne zu fragen: cargo, git, python3, node, QEMU-Tests,
nur in den Bahn-Ordnern unter `/var/tmp/claude-v2/`. Dateien ändern dürfen sie nur dort. Kein sudo, kein
Internet außer der Anthropic-API. Die Sicherheitsautomatik der App hat Claude deshalb nicht erlaubt, ihn selbst
zu starten. Ob du ihn startest, entscheidest du.

Konsole öffnen: Windows-Taste, „Konsole“ tippen, Enter. Befehle einzeln einfügen mit **Strg+Umschalt+V**, dann
Enter.

## 1. Guthaben abholen (einmal)

Im Browser auf claude.ai: Einstellungen → Abrechnung (Billing) → Abschnitt „API credits“ → eine Organisation
wählen und die Bedingungen annehmen.

## 2. API-Schlüssel anlegen (einmal)

Auf platform.claude.com: „API Keys“ → „Create Key“ → Name `bitsos-runner` → Schlüssel kopieren (Strg+C).
Den Schlüssel nicht in den Chat einfügen.

In der Konsole, direkt nach dem Kopieren:

```bash
mkdir -p ~/.config/bitsos
```

```bash
wl-paste > ~/.config/bitsos/anthropic-api-key
```

```bash
chmod 600 ~/.config/bitsos/anthropic-api-key
```

## 3. Probelauf (startet keine Agenten, kostet nichts)

```bash
cd /home/georg/BitsOS/V2/host/sdk-runner
```

```bash
node extract.mjs ../../plan/workflows/a1-wave.js items.json
```

```bash
.venv/bin/python runner.py --dry
```

Er zeigt je Strang, welche Punkte neu, gebaut oder fertig sind.

## 4. Starten, wenn dein Abo-Limit erreicht ist

Erst starten, wenn Claude in der App „Limit erreicht“ meldet. Sonst arbeiten zwei Bauer im selben Ordner.

```bash
.venv/bin/python runner.py --budget 190
```

Die Konsole offen lassen. Der Läufer schreibt seinen Stand nach `out/runner.log`, die Berichte der Agenten nach
`out/`. Anhalten mit **Strg+C**. Ein neuer Start macht dort weiter.

Meldet der erste Agent „credit balance too low“, zählt das Guthaben nicht für den Läufer. Dann hört er sofort auf
und es ist nichts verloren.
