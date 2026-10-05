#!/usr/bin/env python3
"""Shared strict JSON boundary for manifests, measurements and completion ledgers."""
import json
import math


def reject_constant(value):
    raise ValueError("nonfinite JSON constant: " + value)


def unique_object(pairs):
    obj = {}
    for key, value in pairs:
        if key in obj:
            raise ValueError("duplicate JSON key: " + repr(key))
        obj[key] = value
    return obj


def finite_tree(value, path="$"):
    """JSON may parse 1e999 as inf even when parse_constant is strict."""
    if isinstance(value, float) and not math.isfinite(value):
        raise ValueError("nonfinite nested field at " + path)
    if isinstance(value, str):
        # Escaped lone surrogates are accepted by json.loads but cannot be
        # emitted as UTF-8 reports; reject them at the input boundary.
        value.encode("utf-8", errors="strict")
    if isinstance(value, dict):
        for key, child in value.items():
            key.encode("utf-8", errors="strict")
            finite_tree(child, path + "." + key)
    elif isinstance(value, list):
        for i, child in enumerate(value):
            finite_tree(child, path + "[%d]" % i)


def strict_json(text):
    value = json.loads(text, object_pairs_hook=unique_object, parse_constant=reject_constant)
    finite_tree(value)
    return value
