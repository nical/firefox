#!/usr/bin/env python3
"""Manage reftest expectations in both WPT .ini files and reftest.list files.

Usage:
    reftest-expectations.py fuzzy <test-path> <max-diff-range> <total-pixels-range> [--platform=<cond>]
    reftest-expectations.py expect <test-path> <result> [--subtest=<name>] [--platform=<cond>]
    reftest-expectations.py disable <test-path> <bug-url> [--platform=<cond>]

Tests under testing/web-platform/ use .ini expectation files.
All other tests use reftest.list files found in the test's directory (or a parent).
"""

import argparse
import os
import re
import sys
from pathlib import Path


def find_repo_root():
    path = os.getcwd()
    while path != "/":
        if os.path.exists(os.path.join(path, "mach")):
            return path
        path = os.path.dirname(path)
    return os.getcwd()


def is_wpt_test(test_path):
    cleaned = test_path.lstrip("@")
    return "testing/web-platform/" in cleaned


# ---------------------------------------------------------------------------
# WPT .ini file handling
# ---------------------------------------------------------------------------

def test_path_to_ini_path(test_path):
    test_path = test_path.lstrip("@")
    if test_path.startswith("testing/web-platform/tests/"):
        return test_path.replace("testing/web-platform/tests/", "testing/web-platform/meta/", 1) + ".ini"
    elif test_path.startswith("testing/web-platform/meta/"):
        return test_path if test_path.endswith(".ini") else test_path + ".ini"
    else:
        return "testing/web-platform/meta/" + test_path + ".ini"


def get_filename(test_path):
    name = os.path.basename(test_path.lstrip("@"))
    if name.endswith(".ini"):
        name = name[:-4]
    return name


def parse_ini(content):
    """Parse a WPT .ini file into a structured representation."""
    sections = []
    current_section = None
    current_subsection = None
    current_props = []
    current_sub_props = []
    current_multiline_key = None
    current_multiline_lines = []
    target_props = None

    def flush_multiline():
        nonlocal current_multiline_key, current_multiline_lines
        if current_multiline_key and target_props is not None:
            target_props.append((current_multiline_key, current_multiline_lines))
            current_multiline_key = None
            current_multiline_lines = []

    def flush_subsection():
        nonlocal current_subsection, current_sub_props
        if current_subsection is not None:
            current_props.append(("__subsection__", (current_subsection, current_sub_props)))
            current_subsection = None
            current_sub_props = []

    for line in content.splitlines():
        stripped = line.rstrip()
        if not stripped:
            continue

        m = re.match(r'^\[(.+)\]\s*$', stripped)
        if m and not stripped.startswith("  "):
            if current_multiline_key:
                flush_multiline()
            flush_subsection()
            if current_section is not None:
                sections.append((current_section, current_props))
            current_section = m.group(1)
            current_props = []
            current_subsection = None
            current_sub_props = []
            current_multiline_key = None
            target_props = current_props
            continue

        m = re.match(r'^  \[(.+)\]\s*$', stripped)
        if m:
            if current_multiline_key:
                flush_multiline()
            flush_subsection()
            current_subsection = m.group(1)
            current_sub_props = []
            target_props = current_sub_props
            current_multiline_key = None
            continue

        if current_multiline_key:
            if stripped.startswith("    "):
                current_multiline_lines.append(stripped.strip())
                continue
            else:
                flush_multiline()

        if current_subsection is not None:
            m = re.match(r'^    (\w[\w-]*):\s*(.*)', stripped)
            if m:
                key, val = m.group(1), m.group(2)
                if val:
                    current_sub_props.append((key, val))
                else:
                    current_multiline_key = key
                    current_multiline_lines = []
                    target_props = current_sub_props
                continue
        else:
            m = re.match(r'^  (\w[\w-]*):\s*(.*)', stripped)
            if m:
                key, val = m.group(1), m.group(2)
                if val:
                    current_props.append((key, val))
                else:
                    current_multiline_key = key
                    current_multiline_lines = []
                    target_props = current_props
                continue

    if current_multiline_key:
        if target_props is not None:
            target_props.append((current_multiline_key, current_multiline_lines))
    flush_subsection()
    if current_section is not None:
        sections.append((current_section, current_props))

    return sections


def format_ini(sections):
    lines = []
    for section_name, props in sections:
        lines.append(f"[{section_name}]")
        for key, value in props:
            if key == "__subsection__":
                sub_name, sub_props = value
                lines.append(f"  [{sub_name}]")
                for sk, sv in sub_props:
                    if isinstance(sv, list):
                        lines.append(f"    {sk}:")
                        for cline in sv:
                            lines.append(f"      {cline}")
                    else:
                        lines.append(f"    {sk}: {sv}")
                lines.append("")
            elif isinstance(value, list):
                lines.append(f"  {key}:")
                for cline in value:
                    lines.append(f"    {cline}")
            else:
                lines.append(f"  {key}: {value}")
    lines.append("")
    return "\n".join(lines)


def find_or_create_section(sections, filename):
    for i, (name, props) in enumerate(sections):
        if name == filename:
            return i
    sections.append((filename, []))
    return len(sections) - 1


def set_ini_property(props, key, value, platform=None):
    for i, (k, v) in enumerate(props):
        if k == key:
            if platform:
                condition = f'if ({platform}): {value}'
                if isinstance(v, list):
                    for j, line in enumerate(v):
                        if line.startswith(f'if ({platform})'):
                            v[j] = condition
                            return
                    v.insert(0, condition)
                else:
                    props[i] = (key, [condition, v])
            else:
                props[i] = (key, value)
            return

    if platform:
        props.append((key, [f'if ({platform}): {value}']))
    else:
        props.append((key, value))


def wpt_fuzzy(args, repo_root):
    filename = get_filename(args.test_path)
    ini_path = os.path.join(repo_root, test_path_to_ini_path(args.test_path))

    fuzzy_value = f"maxDifference={args.max_diff};totalPixels={args.total_pixels}"

    sections = []
    if os.path.exists(ini_path):
        with open(ini_path) as f:
            sections = parse_ini(f.read())

    idx = find_or_create_section(sections, filename)
    _, props = sections[idx]
    set_ini_property(props, "fuzzy", fuzzy_value, platform=args.platform)

    os.makedirs(os.path.dirname(ini_path), exist_ok=True)
    with open(ini_path, "w") as f:
        f.write(format_ini(sections))

    print(f"Updated {os.path.relpath(ini_path, repo_root)}")


def wpt_expect(args, repo_root):
    filename = get_filename(args.test_path)
    ini_path = os.path.join(repo_root, test_path_to_ini_path(args.test_path))

    sections = []
    if os.path.exists(ini_path):
        with open(ini_path) as f:
            sections = parse_ini(f.read())

    idx = find_or_create_section(sections, filename)
    _, props = sections[idx]

    if args.subtest:
        sub_idx = None
        sub_props = None
        for i, (k, v) in enumerate(props):
            if k == "__subsection__":
                name, sp = v
                if name == args.subtest:
                    sub_idx, sub_props = i, sp
                    break
        if sub_props is None:
            sub_props = []
            props.append(("__subsection__", (args.subtest, sub_props)))
        set_ini_property(sub_props, "expected", args.result, platform=args.platform)
    else:
        set_ini_property(props, "expected", args.result, platform=args.platform)

    os.makedirs(os.path.dirname(ini_path), exist_ok=True)
    with open(ini_path, "w") as f:
        f.write(format_ini(sections))

    print(f"Updated {os.path.relpath(ini_path, repo_root)}")


def wpt_disable(args, repo_root):
    filename = get_filename(args.test_path)
    ini_path = os.path.join(repo_root, test_path_to_ini_path(args.test_path))

    sections = []
    if os.path.exists(ini_path):
        with open(ini_path) as f:
            sections = parse_ini(f.read())

    idx = find_or_create_section(sections, filename)
    _, props = sections[idx]
    set_ini_property(props, "disabled", args.bug_url, platform=args.platform)

    os.makedirs(os.path.dirname(ini_path), exist_ok=True)
    with open(ini_path, "w") as f:
        f.write(format_ini(sections))

    print(f"Updated {os.path.relpath(ini_path, repo_root)}")


# ---------------------------------------------------------------------------
# reftest.list file handling
# ---------------------------------------------------------------------------

def find_reftest_list(test_path, repo_root):
    """Find the reftest.list that contains the given test file.

    Searches from the test's directory upward for a reftest.list that
    references the test filename.
    """
    cleaned = test_path.lstrip("@")
    abs_test = os.path.join(repo_root, cleaned)
    test_filename = os.path.basename(cleaned)
    test_dir = os.path.dirname(abs_test)

    search_dir = test_dir
    while search_dir.startswith(repo_root):
        candidate = os.path.join(search_dir, "reftest.list")
        if os.path.exists(candidate):
            with open(candidate) as f:
                content = f.read()
            # Check if this reftest.list references our test (directly or via relative path)
            rel_from_list = os.path.relpath(abs_test, search_dir)
            if test_filename in content or rel_from_list in content:
                return candidate
            # Also check for 'include' directives pointing to subdirectories
            # that might contain our test - if we're in a parent dir, keep looking
        if search_dir == repo_root:
            break
        search_dir = os.path.dirname(search_dir)

    # Fallback: use reftest.list in the test's own directory (may need to create it)
    return os.path.join(test_dir, "reftest.list")


def find_test_line(lines, test_filename):
    """Find the line index in reftest.list that references the test.

    Returns (index, line) or (None, None) if not found.
    """
    for i, line in enumerate(lines):
        stripped = line.strip()
        if stripped.startswith("#"):
            continue
        # Match the test filename - it appears after == or != operator
        # Pattern: [annotations...] ==|!= test-file ref-file [# comment]
        if re.search(r'(?:==|!=)\s+' + re.escape(test_filename) + r'(?:\s|$)', stripped):
            return i, line
    return None, None


def remove_annotation(line, annotation_type):
    """Remove all instances of an annotation type from a reftest.list line.

    annotation_type is e.g. 'fuzzy', 'fuzzy-if', 'skip', 'skip-if', 'fails', 'fails-if'.
    """
    # Remove annotations like: fuzzy(...) or fuzzy-if(...)
    # These have balanced parentheses
    pattern = re.escape(annotation_type) + r'\([^)]*\)'
    line = re.sub(pattern + r'\s*', '', line)
    # Also handle bare keywords like 'skip' (no parens)
    if annotation_type in ('skip', 'fails', 'random'):
        line = re.sub(r'\b' + re.escape(annotation_type) + r'\b\s+', '', line)
    return line


def add_annotation_to_line(line, annotation):
    """Prepend an annotation before the == or != operator."""
    m = re.match(r'^(\s*)(.*?)((?:==|!=)\s+.*)$', line)
    if m:
        indent, prefix, rest = m.group(1), m.group(2), m.group(3)
        prefix = prefix.rstrip()
        if prefix:
            return f"{indent}{prefix} {annotation} {rest}"
        else:
            return f"{indent}{annotation} {rest}"
    # Fallback: just prepend
    return annotation + " " + line


def reftest_list_fuzzy(args, repo_root):
    test_filename = os.path.basename(args.test_path.lstrip("@"))
    list_path = find_reftest_list(args.test_path, repo_root)

    if not os.path.exists(list_path):
        print(f"Error: Could not find reftest.list for {args.test_path}", file=sys.stderr)
        sys.exit(1)

    with open(list_path) as f:
        lines = f.readlines()

    idx, line = find_test_line(lines, test_filename)
    if idx is None:
        print(f"Error: Test '{test_filename}' not found in {list_path}", file=sys.stderr)
        sys.exit(1)

    if args.platform:
        annotation = f"fuzzy-if({args.platform},{args.max_diff},{args.total_pixels})"
        # Remove existing fuzzy-if with same condition
        pat = re.compile(r'fuzzy-if\(' + re.escape(args.platform) + r',[^)]*\)\s*')
        line = pat.sub('', line)
    else:
        annotation = f"fuzzy({args.max_diff},{args.total_pixels})"
        # Remove existing unconditional fuzzy (but not fuzzy-if)
        line = re.sub(r'(?<!-)fuzzy\([^)]*\)\s*', '', line)

    line = add_annotation_to_line(line, annotation)
    lines[idx] = line if line.endswith('\n') else line + '\n'

    with open(list_path, "w") as f:
        f.writelines(lines)

    print(f"Updated {os.path.relpath(list_path, repo_root)} line {idx + 1}")


def reftest_list_expect(args, repo_root):
    test_filename = os.path.basename(args.test_path.lstrip("@"))
    list_path = find_reftest_list(args.test_path, repo_root)

    if not os.path.exists(list_path):
        print(f"Error: Could not find reftest.list for {args.test_path}", file=sys.stderr)
        sys.exit(1)

    with open(list_path) as f:
        lines = f.readlines()

    idx, line = find_test_line(lines, test_filename)
    if idx is None:
        print(f"Error: Test '{test_filename}' not found in {list_path}", file=sys.stderr)
        sys.exit(1)

    result = args.result.upper()

    # Map expected results to reftest.list annotations
    annotation_map = {
        "FAIL": ("fails", "fails-if"),
        "RANDOM": ("random", "random-if"),
    }

    if result not in annotation_map:
        print(f"Error: For reftest.list, expected result must be FAIL or RANDOM, got '{result}'", file=sys.stderr)
        print("(reftest.list uses 'fails'/'fails-if' and 'random'/'random-if' annotations)", file=sys.stderr)
        sys.exit(1)

    bare, conditional = annotation_map[result]

    if args.platform:
        annotation = f"{conditional}({args.platform})"
        pat = re.compile(re.escape(conditional) + r'\(' + re.escape(args.platform) + r'\)\s*')
        line = pat.sub('', line)
    else:
        annotation = bare
        line = re.sub(r'\b' + re.escape(bare) + r'\b\s+', '', line)
        line = re.sub(re.escape(conditional) + r'\([^)]*\)\s*', '', line)

    line = add_annotation_to_line(line, annotation)
    lines[idx] = line if line.endswith('\n') else line + '\n'

    with open(list_path, "w") as f:
        f.writelines(lines)

    print(f"Updated {os.path.relpath(list_path, repo_root)} line {idx + 1}")


def reftest_list_disable(args, repo_root):
    test_filename = os.path.basename(args.test_path.lstrip("@"))
    list_path = find_reftest_list(args.test_path, repo_root)

    if not os.path.exists(list_path):
        print(f"Error: Could not find reftest.list for {args.test_path}", file=sys.stderr)
        sys.exit(1)

    with open(list_path) as f:
        lines = f.readlines()

    idx, line = find_test_line(lines, test_filename)
    if idx is None:
        print(f"Error: Test '{test_filename}' not found in {list_path}", file=sys.stderr)
        sys.exit(1)

    bug_comment = f" # {args.bug_url}" if args.bug_url else ""

    if args.platform:
        annotation = f"skip-if({args.platform})"
        # Remove existing skip-if with same condition
        pat = re.compile(r'skip-if\(' + re.escape(args.platform) + r'\)\s*')
        line = pat.sub('', line)
    else:
        annotation = "skip"
        # Remove existing skip/skip-if
        line = re.sub(r'\bskip\b\s+', '', line)
        line = re.sub(r'skip-if\([^)]*\)\s*', '', line)

    # Remove any existing trailing comment about this bug
    line = line.rstrip('\n')
    line = add_annotation_to_line(line, annotation)

    # Add bug comment if not already present and there's a bug URL
    if bug_comment and args.bug_url not in line:
        # Remove trailing newline, add comment
        existing_comment = re.search(r'\s*#.*$', line)
        if existing_comment:
            line = line + f" ({args.bug_url})"
        else:
            line = line + bug_comment

    lines[idx] = line if line.endswith('\n') else line + '\n'

    with open(list_path, "w") as f:
        f.writelines(lines)

    print(f"Updated {os.path.relpath(list_path, repo_root)} line {idx + 1}")


# ---------------------------------------------------------------------------
# Main dispatch
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description="Manage reftest expectation files")
    subparsers = parser.add_subparsers(dest="command", required=True)

    p_fuzzy = subparsers.add_parser("fuzzy", help="Add/update fuzzy annotation")
    p_fuzzy.add_argument("test_path", help="Path to the test")
    p_fuzzy.add_argument("max_diff", help="maxDifference range, e.g. 0-1")
    p_fuzzy.add_argument("total_pixels", help="totalPixels range, e.g. 0-500")
    p_fuzzy.add_argument("--platform", help='Platform condition')

    p_expect = subparsers.add_parser("expect", help="Set expected result")
    p_expect.add_argument("test_path", help="Path to the test")
    p_expect.add_argument("result", help="Expected result: FAIL, etc.")
    p_expect.add_argument("--subtest", help="Subtest name (WPT only)")
    p_expect.add_argument("--platform", help='Platform condition')

    p_disable = subparsers.add_parser("disable", help="Disable a test")
    p_disable.add_argument("test_path", help="Path to the test")
    p_disable.add_argument("bug_url", help="Bugzilla bug URL")
    p_disable.add_argument("--platform", help='Platform condition')

    args = parser.parse_args()
    repo_root = find_repo_root()

    if is_wpt_test(args.test_path):
        if args.command == "fuzzy":
            wpt_fuzzy(args, repo_root)
        elif args.command == "expect":
            wpt_expect(args, repo_root)
        elif args.command == "disable":
            wpt_disable(args, repo_root)
    else:
        if args.command == "fuzzy":
            reftest_list_fuzzy(args, repo_root)
        elif args.command == "expect":
            reftest_list_expect(args, repo_root)
        elif args.command == "disable":
            reftest_list_disable(args, repo_root)


if __name__ == "__main__":
    main()
