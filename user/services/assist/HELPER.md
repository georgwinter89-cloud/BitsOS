# Helper programs for `assist` (provider kind `program`)

A provider of kind `program` is a program on this machine that answers instead of an HTTP service. `assist`
starts it once per answer, hands it the conversation and reads the answer from it. This file is the whole
contract, version 5; the code is `src/provider/program/`. A helper written for version 1 to 4 runs unchanged
(see "Version 1" to "Version 4" at the end): version 2 adds what a helper may do when its model has tools of its
own, which is to tell what they do and to ask the owner first; version 3 adds the picture of a question for a
helper that says it takes pictures (see "Pictures"); version 4 hands the helper's model the tools `assist` itself
offers for this question, the same an HTTP provider gets (see "assist's tools"); version 5 (plan/spec/
assistent-eins K2) has one level for everything the assistant does, in every chat, and the request names it
(see "The level").

    assist --add NAME
    settings set ai.provider.NAME.kind program
    settings set ai.provider.NAME.address /full/path/of/the/helper
    settings set ai.provider.NAME.place local        (or cloud: see "Place")
    settings set ai.role.chat NAME

`ai.provider.NAME.model` is optional; a helper may use it. There is no key: `assist --key NAME` refuses, and
nothing secret is ever handed to a helper.

## How the helper is started

- The address is the full path of a regular, executable file (a link to one is fine). No shell, no arguments:
  a helper that needs options is a small script that starts the real program with them.
- Environment and working directory are those of `assist`.
- It runs in a process group of its own. The whole group gets SIGTERM, and with it every process that
  descends from the helper and left the group (a command in a session of its own), and SIGKILL once all are
  gone or three seconds later, when the answer is complete and the helper has not ended by itself within two seconds, when
  the helper was silent for `ai.timeout_s` (see "Bounds"), when the user presses Esc, and when the helper
  breaks this contract. When `assist` itself is killed, the kernel ends the helper (not its children: a
  helper that starts programs should end them when its own stdout closes).
- Standard error goes to `assist`'s log (`--log FILE` in the chat, stderr of `assist --once`), never to the
  screen: at most 200 lines of 400 characters per answer, the rest is read and dropped.

## The request: one line on stdin

One line of JSON (UTF-8), then stdin is closed:

    {"bitsos_helper":5,"provider":"NAME","model":"","max_tokens":64000,"system":"...","level":"ask","messages":[
      {"role":"user","text":"..."},{"role":"assistant","text":"..."},
      {"role":"user","text":"...","image":{"media_type":"image/png","data":"iVBORw0KGgo..."}}],
     "bell":"/run/user/1000/assist-bell-6f0c.../s","system_tools":"...","home":"/data/home/1000",
     "allow":{"read":"always","write":"ask","run":"ask","net":"ask","other":"ask"},
     "tools":[{"name":"settings_get","description":"...","schema":{"type":"object",...},"class":"read"}],
     "tool_socket":"/run/user/1000/assist-tools-9a1e.../s","tools_system":"..."}

(The line breaks above are for reading; the request is one line.)

| Field | Meaning |
|---|---|
| `bitsos_helper` | version of this contract, `5` |
| `provider` | the provider's name in the settings |
| `model` | `ai.provider.NAME.model`, may be empty |
| `max_tokens` | `ai.max_tokens`, the longest answer wanted |
| `system` | the one prompt of every chat without its paragraph about the tools (that is `tools_system`): who the assistant is, the level, the fixed rules, how the view shows text. Use it or not |
| `level` | version 5: the level the question works under, `answer`, `ask` or `auto` (see "The level") |
| `messages` | the whole conversation so far, oldest first; the last entry is the new question. Only that one may carry an `image` (see "Pictures") |
| `bell` | where to ask the owner before a tool of the helper's own does something (see "Asking the owner"). Not there when no bell could be made, or when the question wants none of the helper's own tools (`own_tools`): then run without tools of its own |
| `own_tools` | only when `false` (the level `answer`): give the model no tools of its own, only those of `tools`, which then are the reading ones |
| `system_tools` | a paragraph to add to `system` when the helper gives its model tools: BitsOS asks the owner itself, and what a refusal means. A helper without tools leaves it out |
| `home` | the owner's home folder, inside which reading runs unasked; empty when unknown |
| `allow` | what the level means for each kind of action: `always`, `ask` or `never` (see "Kinds"). A helper tells its model's program so (claude-abo pre-approves the `always` kinds); the bell decides in every case |
| `tools` | version 4: `assist`'s tools for this question, each `name`, `description`, `schema` (JSON Schema of its input) and `class` (`read`, `change`, `other`). Not there when none are offered. A helper whose model has tools of its own (`own_tools` not false) may leave out the native ones it has an equivalent of: `befehl`, `befehl_ausgabe`, `befehl_beenden`, `datei_lesen`, `datei_schreiben`, `datei_aendern`, `dateien_finden`, `text_suchen`, `web_holen` (version 5; never `loeschen`, which keeps the trash) |
| `tool_socket` | version 4: where the model's calls of `tools` go (see "assist's tools"); there exactly when `tools` is |
| `tools_system` | version 4, optional: what the chat's prompt says about these tools, to add to `system` when the helper hands them on |

`role` is `user` or `assistant`. The whole conversation comes with every request; a helper that keeps state
between two starts must check it against `messages` itself. Two entries with the same role can follow each
other. What a helper's tools did is not part of the conversation: only the text of its answers is kept. New
fields may appear in later versions; ignore what you do not know.

## The level (version 5)

One level counts for every tool of every chat (`ai.level`, only the owner loosens it): `answer` (Nur antworten),
`ask` (Fragt vorher) or `auto` (Automodus). A question from the control socket or MCP never gets more than `ask`.
For a helper:

| `level` | its own tools | `allow` | `tools` |
|---|---|---|---|
| `answer` | none: `own_tools` is `false`, no `bell` | read `always`, the rest `never` | the reading ones |
| `ask` | reading runs, everything else is asked at the bell | read `always`, the rest `ask` | all |
| `auto` | everything runs, without a question | all `always` | all |

A helper that pre-approves what is `always` (claude-abo) lets its model's program run those tools without a second
question; in `auto` that is every tool. The fixed rules hold at every level whatever a helper does: the assistant's
process tree cannot write the boot stick's CONFIG.TXT or the key stores, and what it deletes goes to the trash
(plan/spec/assistent-eins K5, K6); `loeschen` and a change of `ai.*` are asked at every level.

## Pictures

A view may send a picture of the window in front with its question (the popup's first question, the sidebar when
it sees the window, plan/spec/assistent-eins K4). A helper gets that picture only when its
description says it takes pictures (see "Telling the settings page about a helper": `bitsos_helper = 3` and
`pictures = yes`). Every other helper gets the question as text only, as before, and the desktop tells the
owner that the answer knows only the window's title.

    {"role":"user","text":"...","image":{"media_type":"image/png","data":"<base64>"}}

| Field of `image` | |
|---|---|
| `media_type` | `image/png` or `image/jpeg`, nothing else |
| `data` | the picture's bytes in base64 (`A-Z a-z 0-9 + / =`), never empty, at most 12 MiB of base64 |

- Only the newest question (the last entry) carries a picture: the newest picture of the conversation, so a
  follow-up about the same window carries it again (a helper need not keep anything between requests). At most
  one picture per line. A helper that keeps it for later stores a picture of the owner's screen: say so in its
  own documentation, keep it private to the owner and not longer than needed.
- `assist` checks type, alphabet and size before it sends; a picture that does not fit is left out. A helper
  that gets one it cannot read says so with `stop` `error` rather than answering as if it had seen it.
- Why a description and not a question to the helper: a view must know before the first question whether a
  picture can go, and a helper of version 1 or 2 would take any line on stdin as its request. Reading the
  description executes nothing.

## assist's tools (version 4)

The tools of `tools` are the ones an HTTP provider would get for the same question (one list for every chat: the
settings and the machine's tools, the native tools, the desktop tools `fenster`, `sehen`, `handeln`, `foto` when a
view shows the chat), already cut to the level (at `answer` only the reading ones). A helper whose model can call
tools hands them on; each call goes to the Unix socket `tool_socket` as one line of JSON, the answer is one line:

    -> {"id":"c1","name":"settings_get","input":{"key":"theme.scale"}}
    <- {"id":"c1","result":"theme.scale = 125 ..."}
    <- {"id":"c2","error":"Georg said no to this call. Do not try it again unless he asks."}
    <- {"id":"c3","result":"ein Foto vom Bildschirm","images":[{"media_type":"image/png","data":"iVBORw0KGgo..."}]}
    -> {"id":"l","list":true}
    <- {"id":"l","tools":[...the same list as in the request...]}

| Field | |
|---|---|
| `id` | the helper's own mark (text or a number), returned with the answer |
| `name` | one of the offered tools; any other name is answered `error` and nothing runs |
| `input` | the call's input, a JSON object checked against the tool's schema |
| `list` | `true`: no call, only the list of the offered tools (for a bridge that builds its own list) |
| `images` | in an answer, version 5: the pictures the call brought (`foto`, `sehen` with a picture), each `media_type` and `data` as in "Pictures"; a helper hands them to its model with the result or leaves them |

`assist` runs every call exactly as an HTTP provider's tool call in the same chat: the same schema check, the same
permission step (the level: a reading tool runs, a changing one asks the owner on the same card at `ask` and runs
at `auto`; a setting that asks first, `ai.*` and `loeschen` ask at every level), the same tool lines on the screen
and the same result. `error` is what the model would read as a failed tool result. The rules around it:

- Several requests may follow each other on one connection, each answered before the next is read; several
  connections may be open (at most 16). Calls run one at a time in the order they arrive.
- Each call counts as one round of `ai.tool_rounds` (`assist` cannot see a helper's turns). One call too many ends
  the answer with the chat's sentence about the limit, as for an HTTP provider. Here a helper differs: an HTTP
  provider's round is a turn, which may hold several calls, so a helper whose model makes several calls in one turn
  reaches the limit sooner than the same model over HTTP.
- Only the helper's own process group is served, and only processes that descend from the helper: a process that
  left the group (a command in a session of its own, `setsid`) or lost its parent is turned away with a line in
  the log. The socket is made for this answer in a folder only this user can enter (mode 0600) and is gone with
  the answer; a call that comes after the answer's last line, or while the answer ends, gets
  `{"error":"the answer has ended; nothing ran"}` and nothing runs. Esc ends the helper and the call it waits for.
- While a call runs (Georg may be deciding about it), the helper's silence clock stands.
- A helper's own tool line (`{"tool":...}`) for one of these calls would show it twice: `assist` shows them itself.
- What a call did is not part of `messages` later (only the text of answers is), as for the helper's own tools;
  a helper that keeps a session of its own keeps the calls there.
- A helper of version 3 or older never reads these fields and runs as before.

A script calls a tool with

    printf '%s\n' '{"id":1,"name":"settings_get","input":{"key":"theme.scale"}}' | assist --call --tool-socket "$sock"

which prints the answer line (one per request line). Like `assist --mcp --bell`, it speaks only to a socket at which
the kernel shows the assistant it descends from listening.

## The answer: lines on stdout

Each line is one JSON object:

| Line | Meaning |
|---|---|
| `{"text":"..."}` | a piece of the answer; shown as it arrives |
| `{"note":"..."}` | optional: a status word for the user (shown as one line of at most 120 characters, not part of the answer) |
| `{"tool":"NAME","state":"runs","text":"..."}` | optional: what a tool of the helper is doing (see "Telling what a tool does") |
| `{}` | optional: nothing to show, only a sign of life (see "Bounds") |
| `{"done":true,"stop":"end","error":null}` | the last line; nothing after it is read |

`stop` says how the answer ended:

| `stop` | |
|---|---|
| `end` | the answer is whole |
| `length` | it was cut at the length limit |
| `refused` | the service declined to answer |
| `error` | something went wrong; `error` is one sentence for the user, **in German** (shown as one line of at most 400 characters). The question is then not kept in the conversation |

Blank lines and objects with none of these fields are skipped. Anything else (a line that is not a JSON
object) ends the answer with an error, and so does closing stdout or ending without the `done` line. The exit
code is not looked at.

## Bounds

- **Silence.** A helper may be silent for `ai.timeout_s` (default 600 s): no line on stdout, no question at
  the bell. Every line of this protocol and every question starts that time anew, and while a question waits
  for the owner the clock stands. There is no limit for the whole answer: an answer that works through tools
  for minutes is what version 2 is for, and what ends a runaway one is the owner's Esc. A helper whose tool
  runs long without printing anything writes `{}` now and then.
- **Memory.** Everything written to stdout for one answer may take a quarter of the machine's free memory.
  Keep tool lines short and never put a tool's whole result into one.

## Telling what a tool does

    {"tool":"Bash","state":"runs","text":"ls -la /data/home/1000/Dokumente"}
    {"tool":"Bash","state":"done","text":""}
    {"tool":"Bash","state":"failed","text":"exit 2: no such file"}

`tool` is the tool's name as the helper calls it (at most 64 characters are shown), `text` one line of what
this call does: the command line, the path, the address (at most 400 characters are shown, the rest is cut
and said so). `state`:

| `state` | |
|---|---|
| `runs` | the call started. Leave it out for a call you asked about at the bell: the answer there already showed it |
| `done` | it ended; an empty `text` keeps the words of the call on screen |
| `failed` | it ended with an error; `text` says which, in one line |

The chat shows these lines as it shows the tools of an HTTP provider. They are for the owner's eyes: nothing
is run because of them, and they are not kept in the conversation or in any log. A state this version does
not know is skipped.

## Asking the owner

Before a tool does something, the helper (or any program it started: the model's own program, an MCP server
of it) rings the bell: it connects to the Unix socket `bell`, writes one line, and reads one line. One
question per connection. The question is read up to its newline; the ringer may then close its writing side
(as `nc -U` does) and still gets its answer. Closing the connection entirely takes the question back.

    -> {"ask":"Bash","id":"call-7","kind":"run","text":"rm /data/home/1000/alt.txt"}
    <- {"id":"call-7","answer":"yes"}
    <- {"id":"call-7","answer":"no","why":"declined","text":"Georg hat Nein gesagt. ..."}

| Field of the question | Meaning |
|---|---|
| `ask` | the tool's name as the helper calls it |
| `id` | optional: the helper's own mark, returned with the answer |
| `kind` | what the call does: `read`, `write`, `run`, `net` or `other` (see "Kinds") |
| `text` | what would happen, as data: the command line, the address, the search words; for `write`, what goes into the file (its size, its start and its end) |
| `path` | for `read` and `write`: the full path of the file or folder |
| `must_ask` | optional, `true`: ask the owner whatever the level allows |
| `words` | optional, `true` (version 5): `text` is the whole question in the asker's own words (BitsOS's own tools ring this way); the level still decides whether it is asked |

| `answer` | `why` | |
|---|---|---|
| `yes` | | run it |
| `no` | `declined` | the owner said no |
| `no` | `unanswered` | the owner did not answer in time (the sidebar waits 120 s) |
| `no` | `never` | the level forbids it (`answer`: a helper's own tools are off) |
| `no` | `cannot_ask` | nobody can be asked where the assistant runs (`assist --once`) |
| `no` | `stopped` | the answer was stopped (Esc), or the question could not be read |
| `no` | `too_long` | the question is too long to be shown whole (see "Kinds") |

`text` of a `no` is a German sentence for the model. Anything but the word `yes`, and a connection that
closes without a line, is a no. `assist` asks one question at a time, in the order they arrive; a second one
waits while the owner decides the first, so do not count on asking side by side. A script asks with

    printf '%s\n' '{"ask":"Bash","kind":"run","text":"ls"}' | assist --ask --bell "$bell"

which prints the answer line (one per question line).

### Kinds, and what the level allows

| `kind` | The call | `answer` | `ask` | `auto` |
|---|---|---|---|---|
| `read` | reads or searches files and folders | no bell | runs (with a `path`) | runs |
| `write` | creates, changes or deletes files | no bell | asked | runs |
| `run` | runs a command or a program | no bell | asked | runs |
| `net` | fetches something from the network, searches the web | no bell | asked | runs |
| `other` | anything else | no bell | asked | runs |

`assist` cannot look into a helper, so the kind is the helper's word. It is taken for the level and for the
words of the question, and no further:

- A kind this version does not know is `other`.
- A `read` without a `path` is asked at `ask`: it cannot be told from a command under that name.
- `must_ask` asks at every level.
- A question that is asked is shown whole or not at all: a `run`, `net`, `other`, `must_ask` or `words` question
  whose `text` is longer than 1200 characters is answered `no` with `why` `too_long` at once (a yes would run what
  the owner could not see). In `auto`, where nothing is shown, length does not matter. Write a long command into a
  file first and run the file.
- The question the owner reads is written by `assist` from the kind and the helper's data ("Befehl
  ausführen: ...", "Lesen: ...", "Datei schreiben oder ändern: ..."), as one bounded line; what is cut is said
  so with the full length. The helper chooses the data, not the sentence (`words` questions excepted, as before
  `must_ask` ones).

A helper is still a program of the owner: nothing here keeps a helper from doing things without asking. The
bell is how an honest helper lets the owner decide what its model may do.

### Who is heard, and what a question costs

- The bell is made anew for every answer, in a folder only this user can enter, with a random name, and is
  gone with the answer. Its name is no secret and no key: the kernel says who rings, and only the helper
  started for this answer and the processes that descend from it are heard (a process whose parent ended
  belongs to nobody and is turned away). Everybody else gets no answer, and a line in the log.
- A ringer has 10 seconds to send its line (at most 256 KiB) and costs one thread until it has its answer; 64
  can wait at once.
- When the ringer goes away before the answer, or the helper ends, the question is taken back from the
  owner's screen and nothing runs. At Esc, when the sidebar closes, and at the end of the answer every
  waiting question is answered `stopped`, the bell closes, and the helper's process group is ended.
- A yes belongs to that one call. It is never kept for a later one.

## BitsOS's own tools for a helper's model

`assist --mcp` is a Model Context Protocol server on stdin and stdout with BitsOS's own tools (the machine's
facts, the settings, starting a program of the library), under BitsOS's permission rules. Run alone, its
tools that change something run for a program that asks its own user (`ai.mcp.ask_tools`, on by default since
DECISIONS 18.11.; such a caller gets at most the level `ask`); while the owner has it off they answer with an error. Started by a helper, or by the
model's program under it, as

    assist --mcp --bell "$bell"

it rings the assistant above with the same question as the chat's (`words`), and the level of that chat decides:
Automodus runs it, Fragt vorher asks the owner on the same card and runs the tool on his yes. It rings only a bell that the kernel shows to be the assistant it descends from.
`--read-only` still means reading only, and the AI settings (`ai.*`) cannot be changed through it at all.

## Place, "Nur lokal", and what a helper may be trusted with

- `place` is the user's word for where the helper sends the conversation. With `ai.local_only` a helper marked
  `cloud` is refused before it is started. BitsOS cannot check what a helper marked `local` really does.
- A helper is a program of the user and sees every conversation it answers. The settings that choose it
  (`ai.role.chat`, `ai.provider.NAME.address`) can be changed by any program of the user, and so can the
  helper's own file when it lies in the user's home.
- If the first provider is a helper that fails before it wrote a piece of the answer, the fallback provider
  (`ai.role.chat_fallback`) is tried.

## Telling the settings page about a helper

An installed helper can describe itself so that the settings page "KI" offers it by name: "Hilfsprogramm:
NAME" with one click "Übernehmen" (a new provider of kind `program` with the helper's path, and its place when
the description names one), then "Fürs Gespräch nehmen" (it becomes `ai.role.chat`; when another provider
answers there, the page asks first). The owner types no path.

The description is a small text file in one fixed folder of the owner's home:

    ~/.local/share/bitsos/helpers/NAME.helper

`NAME` (a-z, 0-9, `-`, `_`) is the provider name it gets (with `-2`, `-3` ... when that name is taken). The
file holds `key = value` lines; `#` starts a comment, unknown keys are skipped:

    bitsos_helper = 3
    name = Mein Helfer
    about = Antwortet über ein Programm auf diesem Rechner.
    path = /full/path/of/the/helper
    place = cloud
    models = fable: Fable, opus: Opus
    pictures = yes

| Key | |
|---|---|
| `bitsos_helper` | the version of the description, `1` to `4` (version 5 of the request needs no new word here); required |
| `name` | the name the page shows, one line, at most 60 characters; required |
| `about` | one German line what it is, at most 160 characters; optional |
| `path` | the full path of the helper, a file that can be run; required |
| `place` | `cloud` or `local`: where the conversation goes. Optional; without it the owner sets the place |
| `models` | the models the sidebar's model list offers, `value: label` separated by commas; optional (see below) |
| `pictures` | `yes`: the helper takes the picture of a question (see "Pictures"); counts only with `bitsos_helper = 3` or later. Anything else, or no line: text only |

What the page does with the folder, and why:

- It reads the folder when the page opens and on "Neu suchen", never more than 32 files of at most 4 KiB.
- **Nothing is executed to list a helper.** The program at `path` is only looked at (a regular file that can
  be run, links followed, as `assist` checks it before it starts one).
- The folder and each file must belong to the owner (or root) and be writable by nobody else; a file must not
  be a link. Otherwise another user could put a program on the owner's page.
- A file that breaks any rule (no version, an unknown version, no name, a path that is not a program, a wrong
  place, too long, not a plain file) is not listed; bitscomp's log names the file and why. Nothing else happens.
- `models`: the value is what the provider's model setting gets when the owner picks it, and so the `model` of
  the request; the label is what the list shows. A value is 1 to 64 of `a-z A-Z 0-9 . _ - / @ [ ]` and does not
  start with `-` (a helper that hands it on as an option's value cannot have it read as an option); a label is one
  line of at most 40 characters without `,` or `:`; at most 12 entries, no value twice. The list always begins with
  "Standard" (an empty model: the helper's own default) and shows a model set by hand that is none of these as it
  stands. A `models` line that breaks a rule is ignored as a whole and logged (`bitscomp: helpers: X.helper: its
  models line is ignored: why`); the rest of the description still counts, and only the first `models` line is
  read. The list matches a provider to its description by the provider's path (the same file, links followed).
- `assist` reads the same folder by the same rules (owner only, no link, at most 32 files of 4 KiB, nothing
  executed) to know whether a provider's helper takes pictures; it matches the provider by its path, links
  followed.
- The description changes nothing by itself: the provider is made only by the owner's click, and every rule of
  the settings (what needs the owner's own yes) is the same as for a provider typed in by hand.

## Version 1

A helper of version 1 knows only `text`, `note` and `done`, reads its request (to the first line break or to
the end of stdin: stdin still closes after the one line) and never rings. It runs unchanged: the new fields
of the request are fields it does not know, and its lines mean what they meant. What changed for it is one
bound: `ai.timeout_s` used to be the longest whole answer and is now the longest silence.

An `assist` of version 1 skips the lines it does not know (`tool`, `{}`), sends no `bell`, and a helper of
version 2 then runs without tools.

## Version 2

A helper of version 2 knows no `image`: it is a field it does not know, and it never gets one unless its
description announces pictures, which a description of version 2 cannot. An `assist` of version 2 never sends a
picture; a helper of version 3 then answers from the text, as every helper did.

## Version 3

A helper of version 3 knows no `tools`, `tool_socket`, `tools_system` or `own_tools`: fields it does not know.
It answers as before, from its own tools if it has any; at the level `answer` it gets no bell (`own_tools` false),
so it answers without tools.

## Version 4

A helper of version 4 knows no `level`, `words` or `images`: fields it does not know. It reads `allow` as before,
which now says what the level means per kind, and runs unchanged.

## A complete helper

    #!/bin/sh
    read -r request
    echo '{"text":"Hallo, "}'
    echo '{"text":"ich bin ein Hilfsprogramm."}'
    echo '{"done":true,"stop":"end","error":null}'

And one whose "model" wants to look into a folder (the path is made up; `sed` takes `bell` out of the
request):

    #!/bin/sh
    read -r request
    bell=$(printf '%s' "$request" | sed -n 's/.*"bell":"\([^"]*\)".*/\1/p')
    answer=$(printf '%s\n' '{"ask":"ls","kind":"run","text":"ls /data/home/1000/Dokumente"}' | assist --ask --bell "$bell")
    case "$answer" in
      *'"answer":"yes"'*) files=$(ls /data/home/1000/Dokumente | tr '\n' ' ')
                          echo '{"tool":"ls","state":"done","text":""}'
                          echo "{\"text\":\"Im Ordner liegen: $files\"}" ;;
      *)                  echo '{"text":"Das durfte ich nicht nachsehen."}' ;;
    esac
    echo '{"done":true,"stop":"end","error":null}'
