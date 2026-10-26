# cx-eval: labelled German test set for ctxd

151 invented documents (no real data: names, numbers and addresses are made up, mail domains are reserved
`.example`, the IBAN is `DE00`), 10 topics, in `docs/` as `<id>.txt|md|eml|html`. Mails use a mix of 8-bit,
RFC 2047 encoded subjects and quoted-printable bodies on purpose (extractor test).

- `topics.json`: per topic the name, `query` (what Georg would type), `query_alt` (a paraphrase), `relevant` ids,
  `near_distractors` (share words, belong elsewhere), `hard` (relevant, but none of the query's words occur).
  A document may be relevant to two topics (for example `steuer-12` is tax and bank).
- `check.py`: validates the set (ids, parsing, reserved domains, overlaps, counts). `--update` rewrites `hard`.
- `baseline.py`: BM25 reference, capped recall@10 per topic. The hybrid ranking must beat it clearly.

Ids: `<topic>-NN` for the documents made for a topic, `misc-NN` for distractors that belong to no tested topic.
Metrics are defined in `user/services/ctxd/DESIGN.md` section 11.
