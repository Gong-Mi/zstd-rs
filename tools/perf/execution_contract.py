#!/usr/bin/env python3
"""Execution identity contract. No performance thresholds or acceptance policy."""
import json
from pathlib import Path

SIDES = ('base', 'head', 'base2')
PROTOCOL = 'rotating-aba-v1'


def schedule(rounds):
    result = []
    for number in range(1, rounds + 1):
        shift = (number - 1) % len(SIDES)
        order = SIDES[shift:] + SIDES[:shift]
        for side in order:
            result.append({'ordinal': len(result) + 1, 'round': number, 'side': side})
    return result


def unique_fields(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError('duplicate completion field: ' + key)
        result[key] = value
    return result


def reject_constant(value):
    raise ValueError('nonfinite completion value: ' + value)


def validate_execution(declaration, rounds, builds, path):
    """Fail closed for new declarations; legacy manifests stay explicitly legacy."""
    errors = []
    if not isinstance(declaration, dict) or declaration.get('protocol') != PROTOCOL:
        return ['execution: unknown protocol']
    affinity = declaration.get('affinity')
    if (not isinstance(affinity, list) or len(affinity) != 1 or
            type(affinity[0]) is not int or affinity[0] < 0):
        return ['execution: singleton nonnegative CPU affinity required']
    expected = schedule(rounds)
    if json.dumps(declaration.get('schedule'), sort_keys=True) != json.dumps(expected, sort_keys=True):
        errors.append('execution: declared schedule is not rotating A/B/A')
    try:
        lines = Path(path).read_text(encoding='utf-8').splitlines()
        rows = [json.loads(line, object_pairs_hook=unique_fields, parse_constant=reject_constant)
                for line in lines if line.strip()]
    except (OSError, ValueError, UnicodeError) as exc:
        return errors + ['execution: cannot read completion ledger: ' + str(exc)]
    if len(rows) != len(expected):
        errors.append('execution: completion count differs from declared schedule')
    for index, row in enumerate(rows):
        if index >= len(expected):
            errors.append('execution: extra completion record')
            continue
        wanted = expected[index]
        if not isinstance(row, dict):
            errors.append('execution: completion must be object')
            continue
        if any(type(row.get(k)) is not int or row[k] != wanted[k] for k in ('ordinal', 'round')):
            errors.append('execution: ordinal/round mismatch')
        if row.get('side') != wanted['side']:
            errors.append('execution: completed side order mismatch')
        observed_cpu = row.get('affinity')
        if (not isinstance(observed_cpu, list) or len(observed_cpu) != 1 or
                type(observed_cpu[0]) is not int or observed_cpu != affinity):
            errors.append('execution: completed affinity drift')
        build = builds.get(wanted['side'], {})
        if not isinstance(build, dict):
            build = {}
        for field in ('source_sha', 'binary_sha256'):
            if row.get(field) != build.get(field):
                errors.append('execution: completed ' + field + ' mismatch')
    return errors
