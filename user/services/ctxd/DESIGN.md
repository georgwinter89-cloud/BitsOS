# ctxd: the context service ("context instead of folders")

Design note CX-0. Lane CX. Status: CX-1 prototype built on CachyOS (15.10.); section 14 lists where it differs
from this note. Open questions 1-4 of section 13 are decided (desk 6a, Georg 15.10.).
Sources: `plan/V2-PLAN.md` §0 (P5, P6), §1a (B1-B8), §8a.4; `plan/V2-LANES.md` §4 (CX-1..CX-7).
Test set and measuring: `host/cx-eval/` (section 11).

Georg's picture: he asks for "Steuererklärung 2025" and the desktop shows the Lohnsteuerbescheinigung, the
Spendenquittungen, the Handwerkerrechnung, the letter from the Finanzamt, grouped, each with a "why is this here".
He never filed them into folders. What he opens together, the system learns to show together. Everything stays on
his machines, he can see all of it, and he can reset all of it.

## 1. What ctxd is, and where it runs

- A user-space **Linux program** (Rust, static musl for V2, the same source on CachyOS). It is not in the kernel:
  extraction of hostile files and a 100 MB-1 GB model must end one process, never the kernel (§8a.4).
- **B2, a package:** `/apps/ctxd/<version>/` (read-only: binary, `manifest.toml`, the default exclude list, no
  model) and **one data folder** `/data/appdata/ctxd/` (section 4). It touches nothing else. Removing ctxd =
  package + data folder + its settings keys + its permissions + its actions. The model files live in the data
  folder (`models/`), not in the package, because they are large and replaceable.
- **Started by `init`** before the desktop, supervised and restarted. A crash costs at most the last unflushed
  batch, never the index (section 4.3).
- **Settings:** the keys live under `context.*` (P2, §8a.4; registered with `settingsd`, readable and writable by
  talking). B2's default would be `app.ctxd.*`; the plan text already says `context.*` for this first-class
  service, so the design uses `context.*` and asks the desk to confirm (open question 1).
- **Scheduling class (B4):** the query path runs `Normal`, extraction and embedding run `Background`, with a
  setting for how many cores indexing may use (`context.index.threads`, default half of the cores). A query never
  waits for indexing: it reads the last committed index snapshot (section 4.3).
- **Prototype (CX-1..4, CachyOS):** same binary, config in `$XDG_CONFIG_HOME/ctxd/`, data in a directory given by
  `--data`, roots given by `--root`. Ports from the lane range 29500-29999: MCP/HTTP on `127.0.0.1:29510`, the web
  UI on `127.0.0.1:29520`. Never Georg's real files without his copy handover (V2-LANES §4 vision lanes).

## 2. Interfaces

Three front doors over **one set of operations**; the operations are written once (`ctxd-core::api`) and the doors
are thin:

1. **Unix socket** `/run/bitsos/ctxd.sock` (V2; on CachyOS `$XDG_RUNTIME_DIR/ctxd.sock`): JSON lines, one request
   or response per line, UTF-8, for the desktop (`bitscomp` context bar), the CLI `ctxd`, and the UI server.
2. **MCP tools** registered through `/run/bitsos/mcp.sock` (MC-6) so Claude has one endpoint (§8a.3). In the
   prototype ctxd also serves MCP itself on `127.0.0.1:29510` (Streamable HTTP, JSON-RPC 2.0, as `spec/net.md`
   §5.1), so Claude Code on the Gaming-PC can already call the tools.
3. **Actions in the manifest (B5):** the same five operations are declared as actions of the program (name, German
   and English description, JSON schema, safety class), served through `libbitsact` (AC-2). The MCP tool and the
   action are the same handler.

### 2.1 Socket protocol

Request: `{"id": 7, "op": "search", "args": {...}}`. Response: `{"id": 7, "ok": true, "result": {...}}` or
`{"id": 7, "ok": false, "error": {"code": "not_found", "message": "..."}}`. Events pushed to a subscriber
(`{"op": "subscribe"}`) have no id: `{"event": "index_progress", ...}`, `{"event": "workspace_changed", "workspace": "ws_..."}`.
Every request has a deadline (`deadline_ms`, default 300 for `search`); when it expires, `search` returns what it
has with `"partial": true` and the reason (for example `"embedder_slow"`, only the keyword part ran).
Error codes: `bad_request`, `not_found`, `denied` (B3 said no), `busy`, `embedder_unavailable` (only when no
fallback is possible), `internal`. Unknown fields are ignored, unknown ops give `bad_request` (hostile input never
panics; a line longer than 1 MiB is answered with `bad_request` and the connection stays usable).

Identifiers: `doc` = `d_<26 chars>` (stable across renames, see 4.2), `workspace` = `ws_<26 chars>`, both
random, never derived from a path.

### 2.2 The five operations (MCP tools)

| Operation / MCP tool | Class (B5) | Arguments | Result |
|---|---|---|---|
| `context_search` | read | `query` (string, required), `limit` (default 10), `kinds` (filter: `mail`, `pdf`, `text`, `image`, ...), `roots` (filter), `since` (ISO date), `explain` (bool, default true), `snippets` (bool, default from `context.claude.snippets` when the caller is MCP) | `items: [{doc, title, path, kind, modified, score, snippet?, why: [{part, text}]}]`, `partial?`, `took_ms`, `embedder` |
| `workspace_open` | change | `name` (string) or `workspace` (id) or `query` (string; creates a workspace from a search), `limit` | `workspace: {id, name, pinned, created, groups: [{label, items: [...]}]}`; creating one is the only effect on state; it also opens a **session** (section 7) |
| `workspace_add` | change | `workspace`, `docs: [doc ids]` or `paths: [path]`, `group?` | `added`, `skipped: [{path, reason}]`; items added by hand count as strong positive feedback |
| `workspace_list` | read | `pinned_only?`, `query?` | `workspaces: [{id, name, pinned, items, last_used}]` |
| `dismiss` | change | `workspace`, `doc`, `scope` (`workspace` default, `query`, `everywhere`), `undo?` (bool) | `ok`; the item leaves the workspace and loses weight (section 7.3); `undo: true` brings it back |

Additions the design asks the desk to accept (they follow from "Georg can see and reset everything", section 9):
`context_status` (read: roots, counts, queue length, embedder in use, last errors), `context_explain` (read: the
full ranking breakdown for one result), `context_forget` (**delete_overwrite**: scope `document`, `workspace`,
`usage`, `index`, `all`; default `ask` by B3/P1), `workspace_rename`, `workspace_pin` (change). Group labels come
from document kinds ("Briefe", "Belege", "Formulare", "Mails") by a small rule table first, later from a model.

Safety classes decide only the B3 default. `read` and `change` are `allow`; `context_forget` and any call that
would write outside ctxd's data folder are `ask`. No ctxd operation ever changes, moves or deletes one of Georg's
files. "Save into this workspace" (CX-6) is a desktop operation that copies a file into the data tree and then
calls `workspace_add`; ctxd itself stays read-only on his files.

## 3. Sources and permissions (B3)

- **Roots** are directories Georg names (`context.roots`, a list; on V2 the data disk's document trees, on
  CachyOS a copy handed over to the lane). Nothing is indexed that is not under a root. ctxd's package manifest
  asks for: read on the roots, read/write on its data folder, connect to the configured embedder endpoint, listen
  on its socket. `policy::check` sees every one of these; the default for everything else is `deny` and shows up
  in the permission view.
- **Never the Windows disk (R8a).** ctxd refuses a root that (a) resolves, after following symlinks and `..`,
  into a mount whose file system is `ntfs`/`ntfs3`/`fuseblk`/`exfat` or whose block device carries a Windows or
  EFI partition, (b) lies under `/run/media`, `/mnt/windows*`, `/media`, or any path listed in
  `context.forbidden_roots`, (c) contains such a mount below it (the walk does not cross mount points unless Georg
  names the other mount as its own root). On V2 the block layer already refuses writes to that disk; ctxd also
  refuses to *read* it, because Georg's rule is about the disk as a whole. A host test creates symlink escapes and
  bind-mount look-alikes (`host/tests/cx/`) and expects `denied`.
- **Default exclude list** (visible and editable, P1, not hidden): `.ssh`, `.gnupg`, `*.kdbx`, `*.pem`, `*.key`,
  browser profile folders, `node_modules`, `.git` objects, `*.tmp`, files over `context.index.max_file_mb`
  (default 64). Georg's decision overrides, never a vendor's.
- **Reading is by the kernel's file events and by walking,** never by watching every directory (FS-9: fanotify
  subset with `FAN_MARK_FILESYSTEM`, create, modify, close-write, move, delete, **open** with the opener's pid).
  Until FS-9 exists (and on the prototype without privileges) a periodic reconcile scan (mtime, size, inode) and
  `inotify` on the roots do the job.

## 4. Storage layout on `/data`

Everything below `/data/appdata/ctxd/`, nothing elsewhere. The data volume is snapshotted (B1), so the whole
folder can be taken back to yesterday 15:00; the layout is made so that any consistent snapshot is a valid state.

```
/data/appdata/ctxd/
  FORMAT                  one line: "ctxd-data 1"  (a newer binary migrates, an older one refuses)
  config/
    roots.toml            roots, excludes (also mirrored in the settings registry; the registry wins)
  catalog/                the document table (derived, rebuildable)
    catalog.log           append-only records, checksummed, replayed at start
    catalog.snap          last compacted state, written to a temp file and renamed
  text/                   extracted text per document, one file per content hash (derived)
    ab/abcdef....txt.zst  zstd; deleted with the document
  index/
    CURRENT               name of the live generation, written by rename
    g000123/
      bm25/               keyword index (tantivy segments)
      vec-<embedder-id>/  vectors + graph for one embedder (section 6.3)
        vectors.f32       chunk vectors, fixed dim, memory-mapped
        chunks.tbl        chunk id -> (doc, byte range, title offset)
        hnsw.graph        optional, built above context.index.hnsw_min_chunks
  usage/                  what Georg did (NOT derived: this is the learned part)
    events-000041.jsonl   append-only, one JSON object per line, rotated by size
    edges.snap            compacted co-use edges (derived from events, rebuildable)
  workspaces/
    ws_xxxx.json          one file per workspace, written by temp + rename
    journal.jsonl         every edit with its inverse (undo, B1)
  models/                 embedder model files (GGUF), only what Georg approved
  tmp/                    scratch, emptied at start
```

### 4.1 What is source of truth, what is derived

- **Not derived (the only things that must never be lost):** `usage/events-*.jsonl`, `workspaces/*.json`,
  `workspaces/journal.jsonl`, `config/roots.toml`. They are small, plain text, and readable by Georg.
- **Derived, rebuildable from the roots plus the model:** `catalog/`, `text/`, `index/`. `ctxd reindex` rebuilds
  them; so does a start that finds any of them broken (a checksum mismatch) or `FORMAT` newer than the binary.
  Learned edges are recomputed from the usage events and are not lost by a rebuild.
- No file of Georg's is copied except as extracted text in `text/` (needed for snippets and for re-chunking when
  the model changes). `context.store_text` (default on) can turn that off; then snippets are read from the
  original file at query time and a model change re-reads the roots.

### 4.2 Stable identity

A document is identified by **(file system id, inode, generation)** on ext4 (FS-8 guarantees stable inode
numbers), so a rename or a move keeps its doc id and its learned history. FAT has no stable inode (only a
directory slot): on a FAT root the identity falls back to **content hash + size**, and a rename of a changed file
is seen as delete + create (history is lost for that file; acceptable for the stick, which is not a document
root). The catalog row holds: doc id, path(s) (a hard link gives several), size, mtime, content hash (BLAKE3 of
the bytes), kind, extractor name + version, status (`ok`, `empty`, `failed:<reason>`, `excluded`), chunk range.
Identical content under two paths is one text and one set of vectors with two paths (duplicates are shown once,
"auch in: ...").

### 4.3 Crash safety (the `kill -9` test of CX-1)

- Writers append to a log or write a temp file and rename; nothing is updated in place.
- An index **generation** (`g000123/`) is complete or absent. Indexing builds into the next generation's
  directory and commits by rewriting `CURRENT` through rename after `fsync`. A query opens `CURRENT` once and
  keeps that generation open, so indexing never blocks queries and a crash leaves the old generation intact.
  Incremental changes write small delta segments; a merge happens in the background.
- On start: read `CURRENT`, verify the cheap checksums, replay `catalog.log` past the snapshot, reconcile against
  the file system (mtime/size/inode), and queue what changed while ctxd was down. A torn last line in a `.jsonl`
  file is dropped and logged (`event!` `ctxd.recover.torn_tail`), never fatal.
- No fixed limits (R1): documents, chunks, workspaces and edges are growable structures; the only bounds are
  settings with visible defaults (file size, extraction time) and the disk.

## 5. Extraction pipeline

```
event or scan -> sniff (magic bytes, then extension) -> extractor (child process) -> normalise
  -> metadata -> chunk -> embed (Background) + keyword-index -> commit
```

- **Extractors** (all run in a **child process** with `RLIMIT_AS`, `RLIMIT_CPU` = `context.index.extract_seconds`
  (default 20) and no network; a hostile or broken file ends the child, the document gets `failed:<reason>`, the
  service continues): plain text and Markdown (charset sniffing, UTF-8 first, then Windows-1252/ISO-8859-15 for
  old German files); HTML (visible text, `<title>`, no script); `.eml` (headers incl. RFC 2047 words, MIME parts,
  quoted-printable and base64, charsets; attachments are listed by name, and indexed only if they are of a
  supported kind and Georg turned `context.index.attachments` on); PDF text layer (the CX-1 builder compares a
  Rust crate against `pdftotext` on the host: `pdftotext` is on the Gaming-PC, but V2 must not depend on it, so
  this is a measured choice); DOCX/ODT (zip + XML, paragraphs and table cells). Images and scanned PDFs: nothing
  in CX-1; CX-7 adds tesseract (German) and photo metadata. Unknown kinds are indexed by file name and path only.
- **Normalise:** Unicode NFC, collapse whitespace, strip control characters, keep line breaks as paragraph hints;
  detect language (de/en, a trigram profile; the model is multilingual, language only chooses stop words).
- **Metadata:** title (document title, `Subject`, first heading, else file name without extension), from/to/date
  for mails, file name, path segments, modified time, extractor warnings. Title and file name go into their own
  keyword fields with a higher weight and are prefixed to every chunk before embedding ("Titel: ... Text: ...").
- **Chunking:** paragraph-aware windows of about 256 tokens with 48 overlap (setting), never splitting inside a
  table row or a line of an address block when avoidable; a short document is one chunk. A mail is one chunk per
  message body plus one for the header line. Chunk count per document is unbounded.
- **Keyword tokenizer** (the evidence is in `host/cx-eval/baseline.py`): lower case, German folding that keeps
  umlauts searchable both ways (`ä`/`ae`, `ß`/`ss`), words kept whole, **plus character 4-grams for words of 5+
  letters** so that a compound meets its parts ("Stromvertrag" finds "Stromliefervertrag" and "Strom"; "Autoversicherung"
  finds "Kfz-Versicherung"). A plain BM25 with whole words or prefixes scores a mean recall@10 of 0.22/0.30 on the
  test set, with 4-grams 0.42/0.34; the model has to bring the rest.
- **Throttling:** `Background` class, `context.index.threads`, and a pause while the machine is busy (the load
  setting, or while Georg's foreground program is in a `Interactive` class under pressure). First index of a
  large tree is allowed to take hours; the UI says how far it is.
- **Idempotent and incremental:** unchanged content hash = no work; a changed file replaces its chunks; a deleted
  file removes its text, vectors and keyword entries within one cycle (this also serves the privacy rule 9.4).

## 6. The embedder

### 6.1 The trait

```rust
pub trait Embedder: Send + Sync {
    /// Identity stored with every vector set: model name, revision, dimension, prompt scheme.
    /// Vectors of different identities are never compared or mixed.
    fn id(&self) -> &EmbedderId;
    fn dim(&self) -> usize;
    fn max_tokens(&self) -> usize;
    /// Query and passage prompts can differ (e5 wants "query: " / "passage: " prefixes).
    fn embed(&self, kind: EmbedKind, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError>;
    fn health(&self) -> Health; // Ready | Loading | Unavailable { reason }
}
pub enum EmbedKind { Query, Passage }
```

`EmbedError` has `Unavailable`, `TooLong` (the caller chunks again), `Timeout`, `Bad(String)`. Batches are the
unit; an implementation says its best batch size. The trait lives in `ctxd-core`, free of any model code, so the
host tests use a **fake embedder** (hashing of character n-grams into a fixed dimension) that makes the
pipeline, the formula and the learning testable without a model file.

### 6.2 Back ends

| Back end | When | Notes |
|---|---|---|
| `LlamaCpu` | always available once a model file is present; the **primary** | llama.cpp/ggml embedding mode on the CPU, 8 threads, AVX-512 on the Mini-PC's Zen 4. Linked as a C library through a thin `-sys` crate or run as a managed child process (`llama-server --embeddings`); CX-1 picks the one that builds statically against musl (open question 3). Candidates: multilingual-e5-small (118 M, 384 dimensions, fast), bge-m3 (568 M, 1024 dimensions, better), one more picked by CX-2 on the test set and docs/s. |
| `LocalHttp` (was `StrataHttp`) | when `ai.role.search` or `ai.role.jobs` names a **local** provider (DECISIONS.md 21.10.; until CL-5: `context.strata.url`) and it answers | Calls an OpenAI-compatible `/v1/embeddings` at the address Georg gave (this machine, the Gaming-PC or another home machine); cloud providers are never an embedder. **Today Strata does not obviously serve sentence embeddings** (its sources use "embedding" for token embeddings, and it listens on 127.0.0.1:8080 only), so the likely shape is a `llama-server --embeddings` with a larger model next to Strata on the GPU. Opening any port to the home network is a step Georg approves (V2-LANES §1). Documents leave the Mini-PC only toward this configured host (R9, section 9). |
| `Npu` | later, not planned | Phoenix XDNA (1022:1502) needs its own driver, firmware and toolchain. The trait is the only thing prepared. |

### 6.3 Using more than one

- The **primary index** is always built with the CPU model and is always complete: ctxd works with the network
  unplugged (P6).
- A **secondary index** with a better model (bge-m3 on Strata, when reachable) is built in the background into
  its own `vec-<embedder-id>/` directory. When complete, `search` embeds the query with both models and fuses
  both vector results into the vector part of the formula (the larger of the two cosines per document after each
  is normalised). When Strata is away, the query simply uses the primary alone, and `explain` says so. A model
  change (new `EmbedderId`) never invalidates the other index and never needs a stop-the-world rebuild.
- Query embedding always has a fast local path: the CPU model embeds a query in a few tens of milliseconds, so
  the search deadline (300 ms) holds without the network.
- **Vector search:** exact scan (SIMD dot products) while the chunk count is below
  `context.index.hnsw_min_chunks` (default 20000, a speed switch and not a limit), HNSW above it (M 16,
  efConstruction 200, efSearch 64, rebuilt lazily). Vectors stored as `f32`; an `i8` quantised copy is a later
  option if the index outgrows the page cache.
- **Calibration:** at index time ctxd samples random chunk pairs and stores the model's cosine floor and
  spread, because e5-style models give 0.7-0.9 even for unrelated text; the ranking formula (section 7.1) uses the
  calibrated value.

## 7. Ranking and learning

### 7.1 Hybrid score

For a query `q`, a candidate document `d`, and an optional workspace/session `w`:

```
S(d | q, w) = w_v * V(d) + w_b * B(d) + w_c * C(d | w, q) + w_r * R(d) - w_x * X(d | w, q)
```

| Part | Definition | Default weight |
|---|---|---|
| `V` vector | best chunk cosine between the query vector and the document's chunks, calibrated to [0,1]: `(cos - floor) / (ceil - floor)`, clamped; the top `K` documents by vector order are candidates (see *Candidate depth*) | `w_v = 0.45` |
| `B` keyword | BM25 of the best field/chunk (title and file name fields boosted x2.5, 4-gram and whole-word terms together), divided by the largest BM25 among the candidates, so it lies in [0,1]; the top `K` documents by keyword order are candidates | `w_b = 0.30` |
| `C` co-use | how strongly `d` is tied to what Georg has used with this context: `C = 1 - exp(-A / tau)`, `A` = sum of edge weights between `d` and the **anchors** (the documents already in workspace `w`, or, for a plain search, documents Georg opened after earlier similar queries: those whose query vector had cosine >= 0.85 to this one); `tau = 2.0` | `w_c = 0.15` |
| `R` recency | `exp(-ln2 * age_days / T_half)` with `age` since the later of file modification and last open, `T_half = 180` days | `w_r = 0.10` |
| `X` dismissal | 1 if `d` was dismissed in workspace `w`; otherwise `1 - exp(-D / 2)` where `D` is the decayed count of dismissals of `d` for similar queries | `w_x = 0.60` (subtracted) |

Rules around the formula:

- **Gate:** a document must reach `V >= v_min` (default 0.35) or `B >= b_min` (default 0.20) to be a result at
  all. Learning (`C`, `R`) can reorder candidates but cannot create one: a frequently opened but unrelated file
  never appears for a query it does not match. Dismissed-in-this-workspace items are hidden and listed under
  "ausgeblendet" with an undo.
- **Candidate depth (R1):** `K = max(context.rank.candidates, 5 * limit)`, setting default 100, so the depth
  grows with `limit` (which has no cap) and with the group size of `workspace_open`. Chunks are collapsed per
  document (keep the best chunk, count the matching ones) **before** the cut at `K`, so one long document with
  many matching chunks takes one candidate slot, not many. The vector list and the keyword list each give up
  to `K` documents; their union is scored.
- **Renormalisation (cold start and fallbacks):** let `P` be the set of positive parts that are present:
  `V` is present when the embedder answered, `C` when any anchor exists (any recorded usage for this context),
  `B` and `R` always. Each present weight becomes `w'_i = w_i / sum(w_j for j in P)`; absent parts count as 0.
  `w_x` is never renormalised. Cases, with the defaults:
  - full: `P = {V,B,C,R}`, weights unchanged (they already sum to 1);
  - cold start (no usage): `P = {V,B,R}` -> `w'_v = 0.45/0.85`, `w'_b = 0.30/0.85`, `w'_r = 0.10/0.85`;
  - partial (embedder unavailable), with usage: `P = {B,C,R}` -> `w'_b = w_b/(w_b+w_c+w_r) = 0.30/0.55`,
    `w'_c = 0.15/0.55`, `w'_r = 0.10/0.55`;
  - partial and cold: `P = {B,R}` -> `w'_b = 0.30/0.40`, `w'_r = 0.10/0.40`.
  The result is marked `partial` whenever `V` is absent. In partial mode only the `B` gate applies; `R` alone
  can never let a document in, because the gate comes first. No keyword hit and no vector -> empty list, not a
  guess.
- **Score range:** the positive part lies in [0,1], so `S` lies in [-w_x, 1]. Results are sorted on the
  **unclamped** `S` (ties as below); the `score` shown to Georg and returned over MCP is `S` clamped to [0,1].
  CX-2 host tests check: weights in each case above sum to 1, sort order uses unclamped `S`, displayed score is
  in [0,1], and `limit = 500` returns up to 500 results.
- **Document score** is the best chunk's `V` and `B`; a tie is broken by the number of matching chunks, then by
  `R`. Duplicates (same content, several paths) are one result.
- **Grouping** into a workspace: take the top `limit` results, cluster by kind and by embedding neighbourhood
  (single linkage above 0.8 cosine), name the groups by the kind rule table.
- The weights, `tau`, `T_half`, `v_min`, `b_min`, `candidates` are settings (`context.rank.*`) so Georg or Claude can change
  them by talking; the shown defaults are the starting point that CX-2 tunes by grid search on the test set
  (section 11.3). A reciprocal-rank-fusion variant is kept as a switch for comparison.

### 7.2 What the system records

All recording is local, in `usage/events-*.jsonl`, schema-versioned, one event per line, **ids and numbers, not
text** (queries are stored only as their vector and a salted hash unless `context.log_queries` is on, because a
query is personal):

| Event | Fields |
|---|---|
| `open` | `ts`, `doc`, `via` (`desktop`, `fanotify`, `mcp`), `program` (ProgramId), `workspace?`, `session` |
| `search` | `ts`, `query_hash`, `query_vec_ref`, `result_ids` (top 20), `workspace?` |
| `add` | `ts`, `workspace`, `doc`, `how` (`manual`, `accepted_suggestion`) |
| `dismiss` | `ts`, `workspace`, `doc`, `scope`, `query_hash?` |
| `undo` | `ts`, `of` (event ref) |

A **session** is the span in which one workspace is open, or, without a workspace, a run of opens with gaps under
`context.session.gap_minutes` (default 20). Opens that are not Georg: ctxd's own reads are never events;
a program that opens more than 20 files within 10 seconds is treated as a scanner (thumbnailer, backup, virus
scan) and its opens are ignored; programs can be put on an ignore list (`context.ignore_programs`).

### 7.3 How the numbers move

- **Co-open edges:** two documents opened in one session get `edge(a,b) += exp(-dt / 300 s)` (opened within
  minutes counts nearly fully, an hour apart almost nothing). Edges decay with a half-life of 90 days when
  compacted into `edges.snap`; an edge below 0.01 is dropped (decay prunes, nothing is capped by count).
- **Manual add** to a workspace counts as 3 co-opens of that document with every item already in the workspace
  (Georg stated the relation himself); **accepting** a suggested item counts as 1.
- **Dismissal** in a workspace hides the document there for good (until undone) and adds `0.5` to its dismissal
  count for queries similar to the one that produced it; `scope: everywhere` adds 2. Dismissals decay with the
  same half-life, so an item can come back after months if it matches strongly (`V` high).
- **Query memory:** an `open` within 10 minutes after a `search` ties the opened document to that query vector
  (used for `C` in a plain search).
- **Replay:** the same code runs on a recorded session file (`ctxd replay sessions.json`) and prints the ranks
  before and after: this is how CX-3 tests learning without a person (section 11.4).

### 7.4 "Why is this here"

Every result carries `why`, built from the same numbers, in German for the desktop and English for logs:

- "Passt zu 'Steuererklärung 2025' (Text: Lohnsteuerbescheinigung, Abschnitt 1)" for `V`/`B`,
- "Oft zusammen geöffnet mit 'Spendenquittung Tierschutz' (7 mal)" for `C`,
- "Zuletzt geändert vor 3 Tagen" for `R`,
- "Von Hand hinzugefügt am 12.03." for a manual add.

`context_explain` returns the raw numbers for each part so Claude can answer "warum steht das da" truthfully.

## 8. Behaviour under B1, B4, B5, B6

- **B1, undoable:** `workspaces/journal.jsonl` records every workspace edit with its inverse; `dismiss undo` and
  `workspace_undo` use it. Settings changes go through `settingsd`'s own journal. The whole data folder is part of
  the `/data` snapshots; restoring a snapshot gives a consistent older state, and the reconcile scan on start
  catches up with files that changed since. Nothing ctxd does needs a stop-the-world step.
- **B4, never freezes the desktop:** the desktop talks to ctxd through the socket with a deadline and keeps
  its last frame when ctxd is slow (the context bar says "sucht noch ..."). ctxd holds no lock while waiting for a
  client. Target (measured, section 11.2): `search` p95 under 150 ms for 100 000 chunks on the Mini-PC while
  indexing runs at `Background`.
- **B5, actions:** manifest actions are the five operations plus `context_status`, `context_explain`,
  `context_forget`, `workspace_rename`, `workspace_pin`, each with a German and English description and a JSON
  schema; the schemas are generated from the Rust argument types so they cannot drift.
- **B6, events, not prose:** `event!(ctxd, ...)` at state changes: `index.started`, `index.finished`
  (`docs`, `chunks`, `seconds`), `doc.extract_failed` (`doc`, `reason`), `doc.excluded` (`reason`),
  `embedder.fallback` (`from`, `to`, `reason`), `embedder.ready`, `search` (`took_ms`, `results`, `partial`,
  never the query text), `recover.torn_tail`, `watch.overflow` (an event queue overflowed, a scan follows),
  `policy.denied` (`access`, `root`). Metrics: `ctxd_docs`, `ctxd_chunks`, `ctxd_queue_len`, `ctxd_embed_docs_per_s`,
  `ctxd_search_ms` histogram, `ctxd_extract_failed_total`. The journal service later answers "why was indexing
  slow last night".

## 9. Privacy rules (P6, R8a, R9)

1. **Local only.** ctxd opens no outgoing connection except to the embedder endpoint Georg configured
   (a local provider he added, until CL-5 `context.strata.url`), which is on his own network. No telemetry, no update check, no model download on its
   own (models arrive by Georg's OK). The fake LAN of the full run (PM-4) must see no other connection from
   ctxd; the connection view shows exactly one possible peer.
2. **Strata is opt-in.** `context.strata.enabled` is off until Georg turns it on. When on, document text (chunks)
   and queries go to that one host; when off, nothing leaves the machine.
3. **Cloud AIs see little.** Whichever cloud provider answers the chat (Claude, ChatGPT, Gemini; DECISIONS.md 21.10.), a result that an MCP tool returns becomes part of that conversation; with `ai.local_only` no cloud provider gets anything, and every cloud handover shows "geht an <Anbieter>" at the ring. So the
   MCP form of `context_search` returns title, path, kind, date and a short snippet (`context.claude.snippet_chars`,
   default 200, 0 turns snippets off) and never full text; a root or a workspace can be marked `private`
   (`context.private_roots`), and its documents are never returned to Claude, they exist only for the desktop. This is Georg's
   call and a visible setting (open question 4); the safety class `read` does not mean "harmless".
4. **Never the Windows disk,** enforced as in section 3 and tested.
5. **Georg sees everything.** `ctxd inspect` (CLI) and a page in the UI list: the roots and exclude rules, counts
   per root and kind, documents that failed and why, the embedder in use, the usage log (last N events, readable),
   the strongest co-use edges ("A und B werden oft zusammen geöffnet"), the dismissals, all workspaces.
   `ctxd export` writes all of it as JSON.
6. **Georg resets everything.** `context_forget` and the UI have separate buttons: one document, one workspace
   (items and learned edges), all learning (`usage/`), the index (`catalog/`, `text/`, `index/`, rebuilt on
   demand), everything. A reset deletes the files (the segments are rewritten without the data), then writes one
   `event!` with counts. A deleted source file removes its text and vectors within one cycle.
7. **No hidden state:** nothing outside `/data/appdata/ctxd/` and the settings registry; logs contain ids and
   numbers, no document text and no query text.
8. **Excluded by default:** secrets-like files (the exclude list of section 3), visible and changeable.

## 10. How it ties into the rest

- **Desktop (CX-6):** the context bar calls `search` with a 300 ms deadline and shows `items` with `why`; saving a
  file calls `workspace_add`; opens in the desktop are reported with `via: desktop`. The pointer-first workspace
  view is A5's design talk, not decided here.
- **File chooser portal (later):** the portal answers from `workspace_list` and `search`; needs the minimal D-Bus
  stand-in decided when Brave runs.
- **Claude client / Claude Code:** the five tools plus status/explain. "zeig mir alles zur Steuererklärung" becomes
  `workspace_open {query: "Steuererklärung"}`.
- **FS-9:** the opener's pid arrives with each open event; ctxd maps it to a ProgramId through the kernel's
  process table (a `/proc/<pid>/` read, or the event itself carrying the id once PM defines it).

## 11. How it will be measured

All numbers come from `host/cx-eval/` (150 synthetic German documents in plain text, Markdown, `.eml`, `.html`;
10 topics; `topics.json`). Georg's own 5-10 topics, once he hands copies over, are a second set that never enters
the repository.

### 11.1 Quality (the A8 gate on the host)

- **capped recall@10** per topic = hits among the top 10 / min(10, number of relevant documents) (a topic with 14
  relevant documents cannot reach more than 10/14 with 10 slots, so the cap keeps the scale fair). Reported for
  both queries of the topic: `query` (what Georg would type) and `query_alt` (a paraphrase with different words),
  and as the mean over topics.
- Also reported: **precision@10**, **nDCG@10**, **MRR** of the first relevant hit, and **near-distractor leakage**
  = average number of `near_distractors` of the topic inside the top 10.
- Hard documents (`hard` in `topics.json`, relevant but containing none of the query's words) are reported
  separately: this is the part that only the model can win.
- **Floor today:** BM25 with prefix tokens: 0.22 / 0.30; BM25 with 4-grams: 0.42 / 0.34 (`baseline.py`).
- **Proposed targets, to be confirmed by CX-2 when a real model is measured:** mean recall@10 >= 0.75 on `query`
  and >= 0.60 on `query_alt`; no topic below 0.50 on `query`; leakage <= 1.5 per query. If the model alone cannot
  reach this on 150 documents, the target is revised with the numbers, not hidden.
- **Overfitting guard:** the weights are tuned on topics 1-5 and checked on 6-10, then swapped; both halves are
  reported. The documents are small, so the numbers are a compass, not a precision instrument.

### 11.2 Speed and robustness

- **Indexing speed** in documents/s and chunks/s with `taskset -c 0-7` and 8 threads (the Mini-PC budget: 8 Zen 4
  cores; the Gaming-PC's cores are faster and that is stated next to the number), per model; with and without
  Strata (batch size, round trips). A generated corpus of 10 000 and 100 000 documents is built by a script for
  speed and `search` latency (p50/p95), memory and index size; these are not committed.
- **Search latency** p50/p95 at the corpus sizes above, with indexing running at `Background`.
- **`kill -9` during indexing and during a commit:** restart gives the old or the new generation, never a broken
  one, and the final index equals a clean rebuild (checked by comparing result lists for all 20 queries).
- **Hostile input:** truncated PDFs, zip bombs, a 2 GB sparse file, invalid UTF-8, a `.eml` with 10 000 headers,
  a symlink loop, a name with newlines: no panic, `failed:<reason>` for the document, service keeps running.
- **Windows-disk refusal** and symlink escape tests (section 3).

### 11.3 Weight tuning (CX-2)

Grid search over `w_v, w_b` (with `w_r` fixed at 0.10 and the usage terms off) on the test set, scoring mean
recall@10 and leakage; the winner becomes the new default if it beats the shown one by a margin that survives the
swapped halves. `tau`, `T_half`, `v_min`, `b_min` follow.

### 11.4 Learning (CX-3)

Replay files in `host/tests/cx/sessions/` (JSON: a list of searches and opens). Checks, all offline:
(a) after k simulated sessions in which documents A and B are opened together, B ranks higher for a query that
finds A, by at least a set number of places; (b) a dismissed document disappears from the workspace and does not
come back for the same query; (c) a document that matches nothing never appears because it was opened often (the
gate); (d) `ctxd forget usage` returns the ranking to the cold-start order exactly; (e) edge decay: after 180
simulated days the effect is about a quarter.

### 11.5 What Georg tries (CX-4)

Two or three of his real topics as workspaces on the Gaming-PC with the web UI on `127.0.0.1:29520`: does the
list look right, do open and dismiss change the order the next day. His words decide the verdict; the numbers
only tell where to look.

## 12. Build order and what each step needs

| Step | What | Needs |
|---|---|---|
| CX-1 | crawler, extractors, chunking, fake embedder, tantivy + vector scan, socket API, CLI, MCP on 29510, `kill -9` test | crates (tantivy, a mail parser, zip, XML, PDF text, a JSON/serde stack) and a llama.cpp build or binding: **none of these is in the offline cargo cache**, so Georg's OK for downloading crates and model files is needed first (open question 2) |
| CX-2 | real models, weights, Strata back end | model files (multilingual-e5-small, bge-m3), Strata/llama-server on the network |
| CX-3 | usage log, edges, dismissals, replay, reset | CX-1 |
| CX-4 | web UI on 29520 | CX-3 |
| CX-5 | V2 build, `init`, FS-9, MC-6 | FS-8, FS-9, MC-6 |
| CX-6, CX-7 | desktop, OCR | WY-1, DP-5; tesseract data |

## 13. Open questions

1. Settings namespace: `context.*` (this note, as §8a.4 says) or `app.ctxd.*` (B2's default)? Recommendation:
   `context.*`, because ctxd is a first-class system service and Georg says "context", not "ctxd".
2. CX-1 needs crates and, from CX-2, model files. Georg's OK for the downloads (crates.io; two model files of
   about 0.2 GB and 1.2 GB) before CX-1 starts.
3. llama.cpp as a linked library or as a child process: decided in CX-1 by what builds statically against musl
   and by memory use; the trait hides it.
4. Default for what goes to Claude (section 9.3): snippets of 200 characters on, `private` roots excluded.
   Recommendation: on, because "Claude, built into the system" is the point; Georg can set 0 or mark roots
   private. It is his decision.
5. Strata needs a sentence-embedding endpoint it does not seem to have today; a `llama-server --embeddings` next
   to it is the likely route. Opening it to the LAN is Georg's step.

## 14. CX-1: the prototype and where it differs (15.10.)

Code: `user/services/ctxd/` (own cargo workspace; `src/` modules match the sections above). Numbers, how to try
it and open points: `plan/notes/CX-1.md`. Decisions taken in CX-1, each changeable later:

- **llama.cpp as a child process** (question 3): upstream `llama-server --embeddings` (release b11303, CPU,
  x86-64-v3), started and supervised by ctxd, talking HTTP over a Unix socket in `<data>/tmp/` (no TCP port).
  A small `sh` wrapper ends it when ctxd's pipe closes, so a `kill -9` of ctxd leaves no model process. 2 slots
  (`context.embedder.slots`), requests of 8 chunks so a query waits for at most one request while indexing runs.
- **Calibration** (6.3) is measured with **document titles as queries**: floor = median cosine of a title query
  to other documents' chunks, ceil = median cosine to its own document. Passage pairs alone gave a floor of 0.80
  and ceil 1.0 for e5, which gated most true results (recall 0.61 instead of 0.66).
- **Embedder trait** takes a timeout per call (`embed(kind, texts, timeout)`), so `search` keeps its deadline.
- **Storage** (4): one generation holds `catalog.json` (documents, roots, calibration, checksums), `chunks.bin`,
  `vectors.f32` and `bm25/`; there is no separate `catalog.log`. Each run that sees a change writes a whole new
  generation from the caches; the caches (`text/<h>/<hash>.json`, `cache/<embedder>/<h>/<hash>-<sig>.bin`,
  checksummed, temp + rename) are what make a crash cost only the batch in flight. Text is stored as JSON, not
  zstd (no C dependency). Delta segments for very large trees are left to CX-5.
- **Reading** (3): inotify on every directory of the roots wakes the indexer (`context.index.watch`, debounce
  `watch_debounce_ms`), the periodic reconcile scan (`context.index.rescan_seconds`, default 60) stays as the
  safety net, and `reindex` on the socket. Changes are judged by (dev, inode, size, mtime, ctime) in nanoseconds;
  a file hashed within 2 s of a write is re-hashed next time ("racy"). Windows/EFI refusal also covers FAT
  (`vfat`/`msdos`), the ntfs-3g FUSE names, partitions typed EFI/Microsoft/LDM (udev data via sysfs), `/boot` and
  `/efi`. Extraction workers run under `nice 10` (B4 on CachyOS), limit their address space, CPU time per file,
  open files and file writes (RLIMIT_AS/CPU/NOFILE/FSIZE), and end themselves when ctxd dies. **Network
  isolation of the workers is not done on the prototype** (no namespace/seccomp without unsafe code or
  privileges); CX-5 gives the V2 worker no network capability at spawn.
- **Ranking** (7.1): `C` is never present (usage log is CX-3), so the cold-start weights apply; `dismiss` has
  only `scope: workspace` (hides in that workspace, undo); `query`/`everywhere` come with CX-3.
- **MCP** (2, 9.3): `Host` must name the loopback listener (DNS rebinding), `Origin` is refused, the bearer
  token stays optional (`context.mcp.token`). `context_forget` over MCP is refused unless `context.claude.allow_forget` (B3 `ask` has no
  screen yet); `context_status` over MCP shows private roots only as `{"private": true}`. Default MCP deadline
  3 s (Claude is not a pointer), socket default 300 ms as designed.
- **Not yet:** `ctxd inspect`/`export`, the UI (CX-4), Strata (CX-2), attachments, OCR (CX-7).
