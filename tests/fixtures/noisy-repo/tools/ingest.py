# -*- coding: utf-8 -*-
"""Ingest the nightly finance export into the billing database."""

import csv
import json  # noqa: F401
from typing import Dict, List

# Column names in the export.
COLUMNS = ["account", "plan", "units"]


def read_rows(path):
    # type: (str) -> List[Dict[str, str]]
    # Open the file.
    with open(path, newline="", encoding="utf-8") as handle:
        # The export is written by a Windows job with a UTF-8 BOM; csv would
        # otherwise read the first header as "\ufeffaccount" and every row
        # would miss its account.
        first = handle.read(1)
        if first != "\ufeff":
            handle.seek(0)
        # Create a reader.
        reader = csv.DictReader(handle, fieldnames=COLUMNS)
        # Return the rows as a list.
        return list(reader)


def units_by_account(rows):
    totals = {}  # type: Dict[str, int]
    for row in rows:
        # skip header row that sometimes shows up twice when finance re-runs the job
        if row["account"] == "account":
            continue
        totals[row["account"]] = totals.get(row["account"], 0) + int(row["units"])
    return totals
