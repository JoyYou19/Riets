#!/usr/bin/env python3
"""
Generates a JSON file of power-set documents for Corelamo search testing.

Vocabulary: alpha, beta, gamma, delta, epsilon, zeta, eta, theta (n=8)
Produces one document per non-empty subset of the vocabulary -> 2^8 - 1 = 255 docs.
Title = subset words joined by spaces (in vocabulary order).

Usage:
    python3 generate_powerset.py > powerset_docs.json
"""

import json
import itertools

VOCAB = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta"]


def generate_documents(vocab):
    docs = []
    doc_id = 1
    # r = subset size, from 1 to len(vocab)
    for r in range(1, len(vocab) + 1):
        for combo in itertools.combinations(vocab, r):
            docs.append({"id": doc_id, "title": " ".join(combo)})
            doc_id += 1
    return docs


def main():
    docs = generate_documents(VOCAB)
    output = {
        "vocabulary": VOCAB,
        "count": len(docs),
        "documents": docs,
    }
    print(json.dumps(output, indent=2))


if __name__ == "__main__":
    main()