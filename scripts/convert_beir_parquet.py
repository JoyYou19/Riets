#!/usr/bin/env python3

import argparse
import json
from pathlib import Path

import pandas as pd


def first_existing_column(df, names):
    for name in names:
        if name in df.columns:
            return name
    raise ValueError(f"None of these columns exist: {names}. Found: {list(df.columns)}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--infile", type=Path, required=True)
    parser.add_argument(
        "--out",
        type=Path,
        default=Path("crates/core-testkit/fixtures/dbpedia.jsonl"),
    )
    parser.add_argument("--limit", type=int, default=None)
    args = parser.parse_args()

    args.out.parent.mkdir(parents=True, exist_ok=True)

    df = pd.read_parquet(args.infile)

    id_col = first_existing_column(df, ["_id", "id", "doc_id"])
    title_col = first_existing_column(df, ["title"])
    text_col = first_existing_column(df, ["text", "body", "contents"])

    if args.limit is not None:
        df = df.head(args.limit)

    with args.out.open("w", encoding="utf-8") as f:
        for i, row in df.iterrows():
            external_id = str(row[id_col])
            title = "" if pd.isna(row[title_col]) else str(row[title_col])
            body = "" if pd.isna(row[text_col]) else str(row[text_col])

            out = {
                "id": external_id,
                "title": title,
                "body": body,
            }

            f.write(json.dumps(out, ensure_ascii=False) + "\n")

    print(f"wrote {len(df)} docs to {args.out}")


if __name__ == "__main__":
    main()
