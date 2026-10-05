#!/usr/bin/env python3
"""Capture normal-link final codegen, actual compiler commands and the same ELF.

No performance thresholds or estimator changes. Unknown/missing capture fails.
"""
import argparse
import csv
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess

from run_ab import harness_identity


def run(command, *, cwd=None, env=None, timeout=180):
    result = subprocess.run([str(x) for x in command], cwd=cwd, env=env,
                            capture_output=True, text=True, timeout=timeout, check=True)
    return result.stdout


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def diagnostic_env(before):
    result = dict(before)
    if result.get('CARGO_ENCODED_RUSTFLAGS'):
        result['CARGO_ENCODED_RUSTFLAGS'] += '\x1f-C\x1fsave-temps=yes'
    else:
        result['RUSTFLAGS'] = (result.get('RUSTFLAGS', '') + ' -C save-temps=yes').strip()
    result['CFLAGS'] = (result.get('CFLAGS', '') + ' -save-temps=obj').strip()
    result['CC_ENABLE_DEBUG_OUTPUT'] = '1'
    return result


def family(path):
    name = Path(path).name
    if name.startswith(('ruzstd_perf_harness.', 'ruzstd_perf_harness-')):
        return 'consumer'
    if name.startswith(('ruzstd-', 'ruzstd.')):
        return 'codec'
    return None


def final_modules(directory):
    selected = {}
    for path in Path(directory).rglob('*.bc'):
        if family(path) is None:
            continue
        if path.name.endswith('.thin-lto-after-pm.bc'):
            key = str(path).removesuffix('.thin-lto-after-pm.bc')
            selected[key] = (path, 'thin-lto-after-pm')
    for path in Path(directory).rglob('*.rcgu.bc'):
        if family(path) is not None and str(path).removesuffix('.bc') not in selected:
            selected[str(path).removesuffix('.bc')] = (path, 'optimized-rcgu')
    rows = sorted(selected.values(), key=lambda x: str(x[0]))
    if {family(path) for path, _ in rows} != {'codec', 'consumer'}:
        raise ValueError('final codegen must contain both codec and consumer modules')
    return rows


def direct_calls(body):
    calls = []
    for line in body.splitlines():
        if not re.match(r'^\s*(?:%[^=]+?=\s*)?(?:(?:tail|musttail|notail)\s+)?(?:call|invoke)\s', line):
            continue
        match = re.search(r'@("[^"]+"|[^ (]+)\(', line)
        if match:
            calls.append(match.group(1).strip('"'))
    return calls


def functions(path):
    result, body, start, readable = [], [], None, ''
    symbol, name = '', ''
    for number, line in enumerate(Path(path).read_text().splitlines(), 1):
        if start is None:
            if line.startswith('; ') and not line.startswith('; Function Attrs:'):
                readable = line[2:]
            if line.startswith('define '):
                match = re.search(r'@("[^"]+"|[^ (]+)\(', line)
                if match is None:
                    raise ValueError('unrecognized LLVM function definition')
                symbol = match.group(1).strip('"')
                name = readable if '::' in readable else symbol
                start, body = number, [line]
        else:
            body.append(line)
            if line == '}':
                text = '\n'.join(body)
                result.append({'file': str(path), 'line': start, 'end_line': number,
                               'symbol': symbol, 'name': name, 'body': text,
                               'calls': direct_calls(text),
                               'urem': len(re.findall(r'\burem\b', text)),
                               'udiv': len(re.findall(r'\budiv\b', text))})
                start, readable = None, ''
    if start is not None:
        raise ValueError('unterminated LLVM function')
    return result


def c_commands(text, package, compiler):
    commands = []
    for line in text.splitlines():
        if 'running:' not in line:
            continue
        tokens = shlex.split(line.split('running:', 1)[1])
        at = next((i for i, token in enumerate(tokens)
                   if token == compiler or Path(token).name == Path(compiler).name), None)
        if at is None:
            continue
        command = tokens[at:]
        if '-c' not in command or '-o' not in command:
            continue
        source = command[command.index('-c') + 1]
        if not source.endswith('.c') or Path(source).name == 'flag_check.c':
            continue
        cwd = Path(tokens[1]) if tokens[0] == 'cd' else package
        resolved = Path(source) if Path(source).is_absolute() else cwd / source
        if not resolved.is_file():
            raise ValueError('recorded C source is missing: ' + str(resolved))
        commands.append({'argv': command, 'cwd': str(cwd), 'source': str(resolved),
                         'object': command[command.index('-o') + 1]})
    if not commands:
        raise ValueError('no actual C library compilation commands captured')
    return commands


def capture(args):
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    receipt = {'schema': 1, 'status': 'FAILED', 'scope': 'normal-linked codegen, not performance approval'}
    (output / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
    source = run(['git', 'rev-parse', 'HEAD'], cwd=args.subject).strip()
    if re.fullmatch('[a-f0-9]{40}', source) is None:
        raise ValueError('unresolved source identity')
    dirty = run(['git', 'status', '--porcelain', '--untracked-files=all', '--',
                 'ruzstd/src', 'ruzstd/Cargo.toml', 'Cargo.toml'], cwd=args.subject)
    if dirty:
        raise ValueError('dirty production source')
    binary = args.target / 'release/ruzstd-perf-harness'
    identity = json.loads(run([binary, '--identity']))
    harness, lock = harness_identity(args.harness)
    if identity != {'source_sha': source, 'harness_sha256': harness, 'lock_sha256': lock}:
        raise ValueError('actual binary identity mismatch')
    shutil.copy2(binary, output / 'binary')
    versions = {name: run(command).strip() for name, command in {
        'rustc': ['rustc', '-vV'], 'cargo': ['cargo', '-V'], 'C': [args.cc, '--version'],
        'llvm-dis': [args.dis, '--version'], 'opt': [args.opt, '--version'],
        'llvm-objdump': [args.objdump, '--version']}.items()}
    shutil.copy2(args.build_log, output / 'build.log')
    build_text = args.build_log.read_text()
    if 'save-temps=yes' not in build_text or '--emit=dep-info,link' not in build_text:
        raise ValueError('effective normal-link save-temps command not witnessed')
    llvm = output / 'llvm'; llvm.mkdir()
    all_functions, modules = [], []
    for index, (bitcode, stage) in enumerate(final_modules(args.target)):
        destination = llvm / (str(index) + '-' + bitcode.name)
        shutil.copy2(bitcode, destination)
        ir = destination.with_suffix('.ll')
        run([args.dis, destination, '-o', ir])
        run([args.opt, '-passes=verify', '-disable-output', ir])
        all_functions.extend(functions(ir))
        modules.append({'family': family(bitcode), 'stage': stage, 'bitcode': str(destination.relative_to(output)),
                        'sha256': sha256(destination), 'ir': str(ir.relative_to(output))})
    metadata = json.loads(run(['cargo', 'metadata', '--locked', '--format-version', '1',
                              '--manifest-path', args.harness / 'Cargo.toml']))
    package = Path(next(p['manifest_path'] for p in metadata['packages'] if p['name'] == 'zstd-sys')).parent
    cdir = output / 'c'; cdir.mkdir()
    crecords = c_commands(build_text, package, args.cc)
    for index, record in enumerate(crecords):
        original = list(record['argv'])
        command = list(original)
        ir = cdir / (str(index) + '-' + Path(record['source']).stem + '.ll')
        command[0] = args.cc
        command[command.index('-o') + 1] = str(ir)
        command = [arg for arg in command if arg not in ('-c', '-save-temps=obj')]
        command += ['-S', '-emit-llvm']
        env = dict(os.environ, LC_ALL='C')
        for key in ('CC_SHIM_OUT_DIR', 'CC_SHIM_OUT_FILES'):
            env.pop(key, None)
        run(command, cwd=record['cwd'], env=env)
        run([args.opt, '-passes=verify', '-disable-output', ir])
        all_functions.extend(functions(ir))
        assembly = Path(record['object']).with_suffix('.s')
        if not assembly.is_file():
            # clang -save-temps=obj can use the input stem rather than cc-rs's hashed object stem.
            assembly = Path(record['object']).parent / (Path(record['source']).stem + '.s')
        if not assembly.is_file():
            raise ValueError('same-build C assembly missing: ' + str(assembly))
        shutil.copy2(assembly, cdir / (str(index) + '-' + assembly.name))
        record.update(source_sha256=sha256(record['source']), emitted_command=command,
                      ir=str(ir.relative_to(output)), ir_sha256=sha256(ir),
                      same_build_assembly_sha256=sha256(assembly))
    if not all_functions:
        raise ValueError('generated modules contain no function definitions')
    names = subprocess.run(
        [args.cxxfilt], input='\n'.join(f['symbol'] for f in all_functions) + '\n',
        capture_output=True, text=True, check=True, timeout=60).stdout
    demangled = names.splitlines()
    if len(demangled) != len(all_functions):
        raise ValueError('function enumeration/demangle count mismatch')
    for f, name in zip(all_functions, demangled):
        f['name'] = name
    csvbuf = io.StringIO(); writer = csv.writer(csvbuf)
    fields = ('file', 'line', 'end_line', 'name', 'symbol', 'urem', 'udiv')
    writer.writerow(fields)
    for f in all_functions:
        writer.writerow([f[key] for key in fields])
    (output / 'functions.csv').write_text(csvbuf.getvalue())
    nm = run([args.nm, '--defined-only', '--format=posix', output / 'binary'])
    (output / 'symbols.txt').write_text(nm)
    elf_symbols = [line.split()[0] for line in nm.splitlines() if line.split()]
    wanted = [symbol for symbol in elf_symbols if any(token in symbol for token in (
        'decode_and_execute_sequences', 'DecodeBuffer6repeat', 'extend_from_within_unchecked',
        'compress_to_vec', 'start_matching', 'ZSTD_decompressSequences', 'ZSTD_compressBlock_fast'))]
    if not any('decode_and_execute_sequences' in symbol for symbol in wanted):
        raise ValueError('actual ELF sequence entry not found')
    assembly = run([args.objdump, '-d', '--no-show-raw-insn', '--disassemble-symbols=' + ','.join(wanted),
                    output / 'binary'])
    (output / 'hot-functions.asm').write_text(assembly)
    named = {f['symbol']: f['name'] for f in all_functions}
    hot = []
    for f in all_functions:
        if any(token in f['name'] for token in ('decode_and_execute_sequences', 'start_matching',
                                              'compress_to_vec', 'extend_from_within_unchecked')):
            hot.append({key: f[key] for key in fields} | {
                'calls': [named.get(c, c) for c in f['calls']],
                'normalized_body_sha256': hashlib.sha256(re.sub(r'![0-9]+', '!MD', f['body']).encode()).hexdigest()})
    receipt.update(status='COMPLETE', source_sha=source, binary_sha256=sha256(output / 'binary'),
                   identity=identity, versions=versions, modules=modules, c_translation_units=crecords,
                   generated_function_count=len(all_functions), hot_functions=hot,
                   note='Replayed C IR is optimized TU evidence; same-build C assembly/ELF remain authoritative')
    (output / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
    print(json.dumps({'status': receipt['status'], 'source': source, 'modules': len(modules),
                      'c_TUs': len(crecords), 'generated_definitions': len(all_functions)}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for option in ('subject', 'target', 'harness', 'output', 'build-log'):
        parser.add_argument('--' + option, type=Path, required=True)
    for option, default in (('cc', 'clang'), ('dis', 'llvm-dis'), ('opt', 'opt'),
                            ('nm', 'llvm-nm'), ('objdump', 'llvm-objdump'), ('cxxfilt', 'llvm-cxxfilt')):
        parser.add_argument('--' + option, default=default)
    capture(parser.parse_args())


if __name__ == '__main__':
    main()
