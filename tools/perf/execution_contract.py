#!/usr/bin/env python3
"""Execution identity contract. No performance thresholds or acceptance policy."""
import json
from pathlib import Path

from json_contract import strict_json

SIDES = ('base', 'head', 'base2')
# 可选第四侧：basex = base 源在独立路径的第二次构建（跨构建代码生成控制腿）。
SIDES_XB = ('base', 'head', 'base2', 'basex')
PROTOCOL = 'rotating-aba-v1'
PROTOCOL_XB = 'rotating-aba-xb-v1'


def schedule(rounds, sides=SIDES):
    if sides not in (SIDES, SIDES_XB):
        raise ValueError('unknown side set')
    result = []
    for number in range(1, rounds + 1):
        shift = (number - 1) % len(sides)
        order = sides[shift:] + sides[:shift]
        for side in order:
            result.append({'ordinal': len(result) + 1, 'round': number, 'side': side})
    return result


def validate_execution(declaration, rounds, builds, path):
    """Fail closed for new declarations; legacy manifests stay explicitly legacy."""
    errors = []
    if not isinstance(declaration, dict) or declaration.get('protocol') not in (PROTOCOL, PROTOCOL_XB):
        return ['execution: unknown protocol']
    sides = SIDES_XB if declaration.get('protocol') == PROTOCOL_XB else SIDES
    affinity = declaration.get('affinity')
    if (not isinstance(affinity, list) or len(affinity) != 1 or
            type(affinity[0]) is not int or affinity[0] < 0):
        return ['execution: singleton nonnegative CPU affinity required']
    expected = schedule(rounds, sides)
    if json.dumps(declaration.get('schedule'), sort_keys=True) != json.dumps(expected, sort_keys=True):
        errors.append('execution: declared schedule is not rotating interleaved')
    try:
        lines = Path(path).read_text(encoding='utf-8').splitlines()
        rows = [strict_json(line) for line in lines if line.strip()]
    except (OSError, ValueError, RecursionError) as exc:
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
