#!/usr/bin/env python3
"""Keyword-only reference for the test set: BM25 over the documents, no model.

It is the floor the hybrid ranking of ctxd must clearly beat (DESIGN.md section 9).
Metric: recall@10 capped = hits in the top 10 / min(10, number of relevant documents).
"""
import json, math, os, re, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import check as ns_mod

HERE = os.path.dirname(os.path.abspath(__file__))
ns = {"DOCS": ns_mod.DOCS, "text_of": ns_mod.text_of}

STOP = {"und", "der", "die", "das", "in", "im", "ich", "für", "zum", "was", "mit", "den", "dem", "ein", "eine", "wer", "mein", "meinen", "zur", "von", "am", "auf", "ist", "es", "wie", "viel", "dieses", "jahr"}


MODE = "prefix"


def toks(text):
    out = []
    for w in re.findall(r"[a-zäöüß0-9]+", text.lower()):
        if w in STOP or len(w) < 2:
            continue
        if MODE == "prefix":
            out.append(w[:7])  # prefix "stemming" so that Steuer, Steuern, Steuererklaerung meet
        elif len(w) < 5:
            out.append(w)
        else:  # character 4-grams: "Stromvertrag" meets "Stromliefervertrag" and "Strom"
            out.extend(w[i:i + 4] for i in range(len(w) - 3))
    return out


def run():
    files = sorted(os.listdir(ns["DOCS"]))
    docs = {f.rsplit(".", 1)[0]: toks(ns["text_of"](os.path.join(ns["DOCS"], f))) for f in files}
    n = len(docs)
    avg = sum(map(len, docs.values())) / n
    df = {}
    for d in docs.values():
        for w in set(d):
            df[w] = df.get(w, 0) + 1

    def score(q, d):
        s, k1, b = 0.0, 1.2, 0.75
        tf = {}
        for w in d:
            tf[w] = tf.get(w, 0) + 1
        for w in set(toks(q)):
            if w in tf:
                idf = math.log(1 + (n - df[w] + 0.5) / (df[w] + 0.5))
                s += idf * tf[w] * (k1 + 1) / (tf[w] + k1 * (1 - b + b * len(d) / avg))
        return s

    topics = json.load(open(os.path.join(HERE, "topics.json"), encoding="utf-8"))["topics"]
    tot = {"query": 0.0, "query_alt": 0.0}
    for t in topics:
        row = f"{t['id']:8}"
        for key in ("query", "query_alt"):
            top = sorted(docs, key=lambda i: -score(t[key], docs[i]))[:10]
            r = len(set(top) & set(t["relevant"])) / min(10, len(t["relevant"]))
            tot[key] += r
            row += f"  {key} {r:4.2f}"
        print(row)
    print(f"mean     query {tot['query']/len(topics):4.2f}  query_alt {tot['query_alt']/len(topics):4.2f}")



def main():
    global MODE
    for MODE in ("prefix", "ngram4"):
        print(f"-- tokenizer: {MODE}")
        run()


main()
