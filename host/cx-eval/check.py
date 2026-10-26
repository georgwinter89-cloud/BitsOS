#!/usr/bin/env python3
"""Validate the synthetic German test set for ctxd (CX-0).

Usage: check.py [--update]   (--update rewrites the "hard" lists in topics.json)
Exit status 1 on any problem. Only the standard library is used.
"""
import email, email.policy, html.parser, json, os, re, sys, unicodedata

HERE = os.path.dirname(os.path.abspath(__file__))
DOCS = os.path.join(HERE, "docs")
errors = []


def err(msg):
    errors.append(msg)


def norm(word):
    # crude German stem: lower case, strip common endings, so that
    # "Steuererklärung" and "Steuererklärungen" count as the same word
    w = unicodedata.normalize("NFC", word.lower())
    for suf in ("ungen", "ung", "en", "er", "e", "n", "s"):
        if len(w) > len(suf) + 4 and w.endswith(suf):
            return w[: -len(suf)]
    return w


def words(text):
    return {norm(w) for w in re.findall(r"[A-Za-zÄÖÜäöüß0-9]+", text)}


def shares_word(query_words, text):
    # a document is "easy" if a word of it contains a query word (or the reverse):
    # "garten" is found in "Gartenhaus", "Kleingarten" and so on
    ws = [w for w in words(text) if len(w) >= 5 and not w.isdigit()]
    return any(q in w or w in q for q in query_words for w in ws)


class Strip(html.parser.HTMLParser):
    def __init__(self):
        super().__init__()
        self.out = []

    def handle_data(self, d):
        self.out.append(d)


def text_of(path):
    raw = open(path, "rb").read()
    try:
        s = raw.decode("utf-8")
    except UnicodeDecodeError:
        err(f"{path}: not UTF-8")
        return ""
    ext = path.rsplit(".", 1)[1]
    if ext == "eml":
        m = email.message_from_bytes(raw, policy=email.policy.default)
        for h in ("From", "To", "Subject", "Date", "Message-ID"):
            if not m[h]:
                err(f"{path}: header {h} missing")
        body = m.get_body(preferencelist=("plain",))
        return f"{m['Subject']}\n{body.get_content() if body else ''}"
    if ext == "html":
        p = Strip()
        p.feed(s)
        return " ".join(p.out)
    return s


def main():
    update = "--update" in sys.argv
    files = sorted(os.listdir(DOCS))
    ids = {}
    texts = {}
    for f in files:
        i, ext = f.rsplit(".", 1)
        if ext not in ("txt", "md", "eml", "html"):
            err(f"{f}: unexpected type")
        if i in ids:
            err(f"duplicate id {i}")
        ids[i] = f
        t = text_of(os.path.join(DOCS, f))
        texts[i] = t
        if not (80 <= len(t) <= 2500):
            err(f"{f}: length {len(t)} outside 80..2500")
        if re.search(r"https?://", t):
            err(f"{f}: real-looking URL")
        for a in re.findall(r"[\w.+-]+@([\w.-]+)", t):
            if not (a.endswith(".example") or a == "example.org"):
                err(f"{f}: e-mail domain {a} is not reserved")
        if re.search(r"\bDE\d\d[ \d]{16,}", t) and "DE00" not in t:
            err(f"{f}: IBAN-like number that is not DE00")
        if re.search(r"\+?\d{2,4}[ /-]\d{3,}[ -]\d{3,}", t) and "Beitragsnummer" not in t:
            pass  # phone numbers are not used; kept as a hook
    data = json.load(open(os.path.join(HERE, "topics.json"), encoding="utf-8"))
    topics = data["topics"]
    if not (140 <= len(ids) <= 170):
        err(f"{len(ids)} documents, expected about 150")
    if len(topics) != 10:
        err(f"{len(topics)} topics, expected 10")
    claimed = set()
    for t in topics:
        rel, near = set(t["relevant"]), set(t["near_distractors"])
        for i in rel | near:
            if i not in ids:
                err(f"{t['id']}: unknown id {i}")
        if rel & near:
            err(f"{t['id']}: relevant and distractor overlap {sorted(rel & near)}")
        if len(rel) != len(t["relevant"]):
            err(f"{t['id']}: duplicate in relevant")
        if len(rel) < 10:
            err(f"{t['id']}: fewer than 10 relevant documents")
        if len(near) < 4:
            err(f"{t['id']}: fewer than 4 near distractors")
        q = {w for w in words(t["query"]) if len(w) >= 5 and not w.isdigit()}
        hard = sorted(i for i in rel if i in texts and not shares_word(q, texts[i]))
        if len(hard) < 3:
            err(f"{t['id']}: only {len(hard)} hard documents (no query word)")
        if update:
            t["hard"] = hard
        elif t["hard"] != hard:
            err(f"{t['id']}: hard list out of date, run check.py --update")
        claimed |= rel
    if update:
        json.dump(data, open(os.path.join(HERE, "topics.json"), "w", encoding="utf-8"), ensure_ascii=False, indent=1)
        open(os.path.join(HERE, "topics.json"), "a").write("\n")
    for t in topics:
        print(f"{t['id']:8} relevant {len(t['relevant']):2}  near {len(t['near_distractors']):2}  hard {len(t['hard']):2}  query: {t['query']}")
    unlabelled = sorted(i for i in ids if i not in claimed)
    print(f"{len(ids)} documents, {len(claimed)} relevant to some topic, {len(unlabelled)} only distractors/noise")
    for e in errors:
        print("ERROR", e)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
