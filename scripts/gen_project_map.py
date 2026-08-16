#!/usr/bin/env python3
"""gen_project_ai_map.py

Generate one compact project map for AI chat/code agents.

Output:
    PROJECT_AI_MAP.txt

Format:
    AIMAP4
    # M path
    # C kind name@line
    # I owner
    # F name@line>called_fn_ids
    # fn id = zero-based F line order, base36 in calls

Purpose:
    - Let AI identify minimal exact source files to request.
    - Provide function/method call graph.
    - Provide enough structure for navigation.
    - Not source code. Do not create exact patches from map only.
"""

import argparse
import re
import sys
from collections import defaultdict
from pathlib import Path


ROOT_FN_NAMES = {
    "main",
    "resumed",
    "window_event",
    "about_to_wait",
    "handle_main_keyboard_input",
    "handle_editor_keyboard_input",
    "handle_main_mouse_input",
    "handle_main_mouse_wheel",
    "handle_main_cursor_moved",
}

STD_CALLS = {
    "abs", "all", "and_then", "any", "as_bytes", "as_mut", "as_ref", "as_str",
    "borrow", "bytes", "ceil", "chars", "clamp", "clear", "clone", "collect",
    "count", "default", "dedup", "drain", "ends_with", "enumerate", "entry",
    "err", "expect", "extend", "filter", "find", "first", "flat_map", "floor",
    "fold", "for_each", "from", "get", "insert", "into", "is_empty", "is_none",
    "is_some", "last", "len", "lines", "lock", "map", "max", "min", "new",
    "next", "ok", "or_else", "or_insert", "parse", "pop", "pop_back",
    "pop_front", "push", "push_back", "push_front", "read", "recv", "remove",
    "replace", "retain", "round", "send", "set", "skip", "sort", "sort_by",
    "split", "sqrt", "starts_with", "sum", "take", "to_owned", "to_string",
    "trim", "try_recv", "unwrap", "unwrap_or", "unwrap_or_default",
    "with_capacity", "write", "zip",
}

KEYWORDS = {
    "Self", "async", "await", "break", "continue", "crate", "else", "enum",
    "false", "fn", "for", "if", "impl", "let", "loop", "match", "mod", "move",
    "pub", "return", "self", "struct", "super", "trait", "true", "unsafe",
    "use", "where", "while",
}

MAX_TYPE_ITEMS = 10
MAX_RW_ITEMS = 12

FN_RE = re.compile(
    r"^([ \t]*)(pub(?:\s*\([^)]*\))?\s+)?"
    r"(?:async\s+)?(?:unsafe\s+)?fn\s+([A-Za-z_]\w*)([^{]*)\{",
    re.MULTILINE,
)

IMPL_RE = re.compile(r"\bimpl\b[^{]*\{", re.MULTILINE)

TYPE_BLOCK_RE = re.compile(
    r"(?m)^([ \t]*)(pub\s+)?(struct|enum)\s+([A-Za-z_]\w*)[^{;]*(?:\{([^}]*)\}|;)"
)

TYPE_TUPLE_RE = re.compile(
    r"(?m)^([ \t]*)(pub\s+)?struct\s+([A-Za-z_]\w*)[^(;]*\(([^)]*)\)\s*;"
)

QUAL_CALL_RE = re.compile(r"\b([A-Za-z_]\w*)::([A-Za-z_]\w*)\s*\(")
SELF_CALL_RE = re.compile(r"\bself\.([A-Za-z_]\w*)\s*\(")
BARE_CALL_RE = re.compile(r"(?<![:.])\b([A-Za-z_]\w*)\s*\(")

SELF_ACCESS_RE = re.compile(r"\bself\.([A-Za-z_]\w*)\s*(\()?")
SELF_ASSIGN_RE = re.compile(r"\bself\.([A-Za-z_]\w*)\s*=[^=]")
SELF_MUTATE_RE = re.compile(
    r"\bself\.([A-Za-z_]\w*)\."
    r"(push|pop|clear|insert|extend|retain|remove|truncate|sort|drain|push_back|pop_back)\b"
)


def short_path(path):
    return str(path).replace("\\", "/")


def base36(value):
    digits = "0123456789abcdefghijklmnopqrstuvwxyz"
    n = int(value)

    if n == 0:
        return "0"

    out = []

    while n:
        n, rem = divmod(n, 36)
        out.append(digits[rem])

    return "".join(reversed(out))


def module_name(path_text):
    return path_text.removeprefix("src/").removesuffix(".rs").replace("/", "::")


def esc(value):
    text = "" if value is None else str(value)
    text = re.sub(r"\s+", " ", text.strip())
    return (
        text
        .replace("\\", "\\\\")
        .replace("|", "\\p")
        .replace("\n", " ")
        .replace("\r", " ")
    )


def compact_csv(text, max_items):
    if not text:
        return ""

    items = [item.strip() for item in text.split(",") if item.strip()]
    if len(items) <= max_items:
        return ",".join(items)

    kept = items[:max_items]
    return ",".join(kept + [f"+{len(items) - max_items}"])


def compact_type_body(body):
    if not body:
        return ""

    body = re.sub(r"\s+", " ", body)
    body = body.replace("pub ", "").strip()

    if body.endswith(","):
        body = body[:-1]

    return compact_csv(body, MAX_TYPE_ITEMS)


def strip_strings_and_comments(src):
    result = list(src)
    i = 0
    n = len(src)

    while i < n:
        ch = src[i]

        if ch == "/" and i + 1 < n and src[i + 1] == "/":
            j = i
            while j < n and src[j] != "\n":
                result[j] = " "
                j += 1
            i = j
            continue

        if ch == "/" and i + 1 < n and src[i + 1] == "*":
            result[i] = " "
            result[i + 1] = " "
            j = i + 2
            while j < n - 1:
                if src[j] == "*" and src[j + 1] == "/":
                    result[j] = " "
                    result[j + 1] = " "
                    j += 2
                    break
                if src[j] != "\n":
                    result[j] = " "
                j += 1
            i = j
            continue

        if ch == "r" and i + 1 < n and src[i + 1] in ('"', "#"):
            next_i = strip_raw_string(src, result, i)
            if next_i != i:
                i = next_i
                continue

        if ch == "b" and i + 2 < n and src[i + 1] == "r" and src[i + 2] in ('"', "#"):
            next_i = strip_raw_string(src, result, i)
            if next_i != i:
                i = next_i
                continue

        if ch == '"':
            result[i] = " "
            j = i + 1
            while j < n:
                if src[j] == "\\" and j + 1 < n:
                    result[j] = " "
                    result[j + 1] = " "
                    j += 2
                    continue
                if src[j] == '"':
                    result[j] = " "
                    j += 1
                    break
                if src[j] != "\n":
                    result[j] = " "
                j += 1
            i = j
            continue

        if ch == "'" and i + 2 < n:
            j = i + 1
            if src[j] == "\\" and j + 2 < n and src[j + 2] == "'":
                for x in range(i, j + 3):
                    result[x] = " "
                i = j + 3
                continue
            if j + 1 < n and src[j + 1] == "'":
                result[i] = " "
                result[j] = " "
                result[j + 1] = " "
                i = j + 2
                continue

        i += 1

    return "".join(result)


def strip_raw_string(src, result, i):
    n = len(src)
    j = i

    if src.startswith("br", i):
        j += 2
    elif src.startswith("r", i):
        j += 1
    else:
        return i

    hashes = 0
    while j < n and src[j] == "#":
        hashes += 1
        j += 1

    if j >= n or src[j] != '"':
        return i

    end_pat = '"' + ("#" * hashes)
    k = j + 1

    while k < n:
        if src.startswith(end_pat, k):
            end = k + len(end_pat)
            for x in range(i, end):
                if src[x] != "\n":
                    result[x] = " "
            return end
        k += 1

    return i


def extract_body(src, brace_pos):
    depth = 1
    i = brace_pos + 1
    n = len(src)

    while i < n and depth > 0:
        if src[i] == "{":
            depth += 1
        elif src[i] == "}":
            depth -= 1
        i += 1

    return src[brace_pos + 1:i - 1], i


def extract_impl_blocks(src_clean):
    impls = []

    for m in IMPL_RE.finditer(src_clean):
        header = m.group(0)[:-1].strip()
        brace_pos = m.end() - 1
        _, end_pos = extract_body(src_clean, brace_pos)

        owner = ""

        owner_m = re.search(r"\bfor\s+([A-Za-z_][A-Za-z0-9_:]*)\s*$", header)
        if owner_m:
            owner = owner_m.group(1).split("::")[-1]
        else:
            owner_m = re.search(r"\bimpl(?:<[^>]*>)?\s+([A-Za-z_][A-Za-z0-9_:]*)\s*$", header)
            if owner_m:
                owner = owner_m.group(1).split("::")[-1]

        impls.append({
            "start": brace_pos + 1,
            "end": end_pos - 1,
            "owner": owner,
        })

    return impls


def extract_types(src_clean, src_orig):
    types = {}

    for m in TYPE_BLOCK_RE.finditer(src_clean):
        kind = m.group(3)
        name = m.group(4)
        body = m.group(5) or ""
        line_no = src_orig[:m.start()].count("\n") + 1

        types[name] = {
            "id": -1,
            "kind": kind,
            "name": name,
            "line": line_no,
            "body": compact_type_body(body),
        }

    for m in TYPE_TUPLE_RE.finditer(src_clean):
        name = m.group(3)
        body = m.group(4)
        line_no = src_orig[:m.start()].count("\n") + 1

        types[name] = {
            "id": -1,
            "kind": "struct",
            "name": name,
            "line": line_no,
            "body": "(" + compact_type_body(body) + ")",
        }

    return types


def extract_functions(src_clean, src_orig):
    fns = []
    impls = extract_impl_blocks(src_clean)

    for m in FN_RE.finditer(src_clean):
        brace_pos = m.end() - 1
        body, _ = extract_body(src_clean, brace_pos)

        name = m.group(3)
        is_pub = bool(m.group(2) and "pub" in m.group(2))

        sig_start = m.start()
        sig_raw = src_orig[sig_start:m.end()].split("{")[0].strip()
        sig_clean = re.sub(r"\s+", " ", sig_raw)

        ret_m = re.search(r"->\s*(.+?)(?:\s+where\b.*)?$", sig_clean)
        ret = ret_m.group(1).strip() if ret_m else ""

        line_no = src_orig[:sig_start].count("\n") + 1

        owner = ""
        recv = ""

        for impl in impls:
            if impl["start"] <= sig_start < impl["end"]:
                owner = impl["owner"]
                break

        args_m = re.search(r"\(([^()]*)\)", sig_clean)
        if args_m:
            args_text = re.sub(r"\s+", " ", args_m.group(1)).strip()
            if args_text:
                parts = [p.strip() for p in args_text.split(",") if p.strip()]
                if parts and parts[0] in ("self", "&self", "&mut self", "mut self"):
                    recv = parts[0]

        qual = f"{owner}.{name}" if owner else name

        fns.append({
            "id": -1,
            "name": name,
            "qual": qual,
            "ret": ret,
            "is_pub": is_pub,
            "body": body,
            "line": line_no,
            "owner": owner,
            "recv": recv,
            "entry": name in ROOT_FN_NAMES,
            "reads": [],
            "writes": [],
            "call_ids": [],
        })

    return fns


def analyze_reads_writes(body):
    reads = set()
    writes = set()

    for m in SELF_ACCESS_RE.finditer(body):
        name = m.group(1)
        is_call = m.group(2) == "("
        if not is_call:
            reads.add(name)

    for m in SELF_ASSIGN_RE.finditer(body):
        writes.add(f"{m.group(1)}=")

    for m in SELF_MUTATE_RE.finditer(body):
        writes.add(f"{m.group(1)}.{m.group(2)}()")

    reads -= {w.split(".")[0].replace("=", "") for w in writes}

    return sorted(reads), sorted(writes)


def add_call(call_ids, target_id, current_id):
    if target_id is not None and target_id != current_id:
        call_ids.add(target_id)


def resolve_qualified_call(prefix, name, current_fn, module_by_path, fns_by_owner_name, fns_by_module_name):
    results = []

    if prefix == "Self" and current_fn["owner"]:
        results.extend(fns_by_owner_name.get((current_fn["owner"], name), []))
        return results

    results.extend(fns_by_owner_name.get((prefix, name), []))

    if results:
        return results

    for path, mod_name in module_by_path.items():
        if mod_name.endswith(prefix):
            results.extend(fns_by_module_name.get((path, name), []))

    return results


def analyze_calls(current_fn, local_fns_by_name, global_fns_by_name, module_by_path, fns_by_owner_name, fns_by_module_name):
    body = current_fn["body"]
    call_ids = set()

    for m in SELF_CALL_RE.finditer(body):
        name = m.group(1)

        if name in STD_CALLS or name in KEYWORDS:
            continue

        if current_fn["owner"]:
            for target in fns_by_owner_name.get((current_fn["owner"], name), []):
                add_call(call_ids, target["id"], current_fn["id"])

        for target in local_fns_by_name.get(name, []):
            add_call(call_ids, target["id"], current_fn["id"])

    for m in QUAL_CALL_RE.finditer(body):
        prefix = m.group(1)
        name = m.group(2)

        if name in STD_CALLS or name in KEYWORDS:
            continue

        targets = resolve_qualified_call(
            prefix,
            name,
            current_fn,
            module_by_path,
            fns_by_owner_name,
            fns_by_module_name,
        )

        for target in targets:
            add_call(call_ids, target["id"], current_fn["id"])

    for m in BARE_CALL_RE.finditer(body):
        name = m.group(1)

        if name in STD_CALLS or name in KEYWORDS:
            continue

        local_targets = local_fns_by_name.get(name, [])
        if local_targets:
            for target in local_targets:
                add_call(call_ids, target["id"], current_fn["id"])
            continue

        global_targets = global_fns_by_name.get(name, [])
        if len(global_targets) == 1:
            add_call(call_ids, global_targets[0]["id"], current_fn["id"])

    return sorted(call_ids)


def build(src_dir, include_tests):
    if include_tests:
        files = sorted(p for p in src_dir.rglob("*.rs"))
    else:
        files = sorted(p for p in src_dir.rglob("*.rs") if "test" not in p.name)

    file_data = []
    module_by_path = {}

    for path in files:
        src_orig = path.read_text(encoding="utf-8", errors="replace")
        src_clean = strip_strings_and_comments(src_orig)
        sp = short_path(path)

        module_by_path[sp] = module_name(sp)

        file_data.append({
            "id": len(file_data),
            "path": sp,
            "types": extract_types(src_clean, src_orig),
            "fns": extract_functions(src_clean, src_orig),
        })

    next_id = 0

    for fd in file_data:
        for type_name in sorted(fd["types"]):
            fd["types"][type_name]["id"] = next_id
            next_id += 1

        for fn in sorted(fd["fns"], key=lambda item: item["line"]):
            fn["id"] = next_id
            next_id += 1

    global_fns_by_name = defaultdict(list)
    fns_by_owner_name = defaultdict(list)
    fns_by_module_name = defaultdict(list)

    for fd in file_data:
        for fn in fd["fns"]:
            global_fns_by_name[fn["name"]].append(fn)

            if fn["owner"]:
                fns_by_owner_name[(fn["owner"], fn["name"])].append(fn)

            fns_by_module_name[(fd["path"], fn["name"])].append(fn)

    for fd in file_data:
        local_fns_by_name = defaultdict(list)

        for fn in fd["fns"]:
            local_fns_by_name[fn["name"]].append(fn)

        for fn in fd["fns"]:
            reads, writes = analyze_reads_writes(fn["body"])
            fn["reads"] = reads
            fn["writes"] = writes
            fn["call_ids"] = analyze_calls(
                fn,
                local_fns_by_name,
                global_fns_by_name,
                module_by_path,
                fns_by_owner_name,
                fns_by_module_name,
            )

    return file_data


def create_map(file_data):
    lines = [
        "AIMAP4",
        "# M path",
        "# C kind name@line",
        "# I owner",
        "# F name@line>called_fn_ids",
        "# fn id = zero-based F line order, base36 in calls",
        "# index only; request full source before exact patch",
    ]

    file_sections = []
    output_fns = []

    for fd in file_data:
        owner_fns = defaultdict(list)
        free_fns = []

        for fn in sorted(fd["fns"], key=lambda item: item["line"]):
            if fn["owner"]:
                owner_fns[fn["owner"]].append(fn)
            else:
                free_fns.append(fn)

        owner_sections = [(owner, owner_fns[owner]) for owner in sorted(owner_fns)]
        file_sections.append((fd, free_fns, owner_sections))
        output_fns.extend(free_fns)

        for _, fns in owner_sections:
            output_fns.extend(fns)

    fn_output_id_by_internal_id = {
        fn["id"]: output_id
        for output_id, fn in enumerate(output_fns)
    }

    for fd, free_fns, owner_sections in file_sections:
        lines.append(f"M {esc(fd['path'])}")

        for type_name in sorted(fd["types"]):
            type_info = fd["types"][type_name]

            lines.append(
                "C {} {}@{}".format(
                    esc(type_info["kind"]),
                    esc(type_info["name"]),
                    type_info["line"],
                )
            )

        for fn in free_fns:
            lines.append(create_fn_line(fn, fn_output_id_by_internal_id))

        for owner, fns in owner_sections:
            lines.append(f"I {esc(owner)}")

            for fn in fns:
                lines.append(create_fn_line(fn, fn_output_id_by_internal_id))

    return "\n".join(lines) + "\n"


def create_fn_line(fn, fn_output_id_by_internal_id):
    call_ids = [
        base36(fn_output_id_by_internal_id[call_id])
        for call_id in fn["call_ids"]
        if call_id in fn_output_id_by_internal_id
    ]
    suffix = f">{','.join(call_ids)}" if call_ids else ""
    return f"F {esc(fn['name'])}@{fn['line']}{suffix}"


def count_tokens(map_text):
    try:
        import tiktoken

        return len(tiktoken.get_encoding("cl100k_base").encode(map_text))
    except Exception:
        return None


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--src", default="src", help="source dir, default: src")
    parser.add_argument("--out", default="PROJECT_AI_MAP.txt", help="output file, default: PROJECT_AI_MAP.txt")
    parser.add_argument("--exclude-tests", action="store_true", help="exclude files with 'test' in filename")
    parser.add_argument("--include-tests", action="store_true", help=argparse.SUPPRESS)

    args = parser.parse_args()

    src_dir = Path(args.src)
    if not src_dir.exists():
        print(f"ERROR: {src_dir} not found. Run from project root or pass --src.", file=sys.stderr)
        sys.exit(1)

    include_tests = True

    if args.exclude_tests:
        include_tests = False

    file_data = build(src_dir, include_tests)
    map_text = create_map(file_data)

    out_path = Path(args.out)
    out_path.write_text(map_text, encoding="utf-8")

    total_types = sum(len(fd["types"]) for fd in file_data)
    total_fns = sum(len(fd["fns"]) for fd in file_data)
    total_edges = sum(len(fn["call_ids"]) for fd in file_data for fn in fd["fns"])
    token_count = count_tokens(map_text)
    token_suffix = f" tokens_cl100k={token_count}" if token_count is not None else ""

    print(
        f"OK {out_path} | files={len(file_data)} types={total_types} "
        f"fns={total_fns} edges={total_edges}{token_suffix}"
    )


if __name__ == "__main__":
    main()
