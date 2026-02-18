# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import os
import re
import sys

from mach.decorators import Command, CommandArgument, SubCommand


def is_wpt_test(test_path):
    cleaned = test_path.lstrip("@")
    return "testing/web-platform/" in cleaned


# ---------------------------------------------------------------------------
# WPT .ini file handling
# ---------------------------------------------------------------------------


def test_path_to_ini_path(test_path):
    test_path = test_path.lstrip("@")
    if test_path.startswith("testing/web-platform/tests/"):
        return test_path.replace(
            "testing/web-platform/tests/", "testing/web-platform/meta/", 1
        ) + ".ini"
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
            current_props.append(
                ("__subsection__", (current_subsection, current_sub_props))
            )
            current_subsection = None
            current_sub_props = []

    for line in content.splitlines():
        stripped = line.rstrip()
        if not stripped:
            continue

        m = re.match(r"^\[(.+)\]\s*$", stripped)
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

        m = re.match(r"^  \[(.+)\]\s*$", stripped)
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
            m = re.match(r"^    (\w[\w-]*):\s*(.*)", stripped)
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
            m = re.match(r"^  (\w[\w-]*):\s*(.*)", stripped)
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
                condition = f"if ({platform}): {value}"
                if isinstance(v, list):
                    for j, line in enumerate(v):
                        if line.startswith(f"if ({platform})"):
                            v[j] = condition
                            return
                    v.insert(0, condition)
                else:
                    props[i] = (key, [condition, v])
            else:
                props[i] = (key, value)
            return

    if platform:
        props.append((key, [f"if ({platform}): {value}"]))
    else:
        props.append((key, value))


def wpt_fuzzy(test_path, max_diff, total_pixels, platform, repo_root):
    filename = get_filename(test_path)
    ini_path = os.path.join(repo_root, test_path_to_ini_path(test_path))

    fuzzy_value = f"maxDifference={max_diff};totalPixels={total_pixels}"

    sections = []
    if os.path.exists(ini_path):
        with open(ini_path) as f:
            sections = parse_ini(f.read())

    idx = find_or_create_section(sections, filename)
    _, props = sections[idx]
    set_ini_property(props, "fuzzy", fuzzy_value, platform=platform)

    os.makedirs(os.path.dirname(ini_path), exist_ok=True)
    with open(ini_path, "w") as f:
        f.write(format_ini(sections))

    print(f"Updated {os.path.relpath(ini_path, repo_root)}")


def wpt_expect(test_path, result, subtest, platform, repo_root):
    filename = get_filename(test_path)
    ini_path = os.path.join(repo_root, test_path_to_ini_path(test_path))

    sections = []
    if os.path.exists(ini_path):
        with open(ini_path) as f:
            sections = parse_ini(f.read())

    idx = find_or_create_section(sections, filename)
    _, props = sections[idx]

    if subtest:
        sub_props = None
        for i, (k, v) in enumerate(props):
            if k == "__subsection__":
                name, sp = v
                if name == subtest:
                    sub_props = sp
                    break
        if sub_props is None:
            sub_props = []
            props.append(("__subsection__", (subtest, sub_props)))
        set_ini_property(sub_props, "expected", result, platform=platform)
    else:
        set_ini_property(props, "expected", result, platform=platform)

    os.makedirs(os.path.dirname(ini_path), exist_ok=True)
    with open(ini_path, "w") as f:
        f.write(format_ini(sections))

    print(f"Updated {os.path.relpath(ini_path, repo_root)}")


def wpt_disable(test_path, bug_url, platform, repo_root):
    filename = get_filename(test_path)
    ini_path = os.path.join(repo_root, test_path_to_ini_path(test_path))

    sections = []
    if os.path.exists(ini_path):
        with open(ini_path) as f:
            sections = parse_ini(f.read())

    idx = find_or_create_section(sections, filename)
    _, props = sections[idx]
    set_ini_property(props, "disabled", bug_url, platform=platform)

    os.makedirs(os.path.dirname(ini_path), exist_ok=True)
    with open(ini_path, "w") as f:
        f.write(format_ini(sections))

    print(f"Updated {os.path.relpath(ini_path, repo_root)}")


# ---------------------------------------------------------------------------
# reftest.list file handling
# ---------------------------------------------------------------------------


def find_reftest_list(test_path, repo_root):
    """Find the reftest.list that contains the given test file."""
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
            rel_from_list = os.path.relpath(abs_test, search_dir)
            if test_filename in content or rel_from_list in content:
                return candidate
        if search_dir == repo_root:
            break
        search_dir = os.path.dirname(search_dir)

    return os.path.join(test_dir, "reftest.list")


def find_test_line(lines, test_filename):
    """Find the line index in reftest.list that references the test."""
    for i, line in enumerate(lines):
        stripped = line.strip()
        if stripped.startswith("#"):
            continue
        if re.search(
            r"(?:==|!=)\s+" + re.escape(test_filename) + r"(?:\s|$)", stripped
        ):
            return i, line
    return None, None


def add_annotation_to_line(line, annotation):
    """Prepend an annotation before the == or != operator."""
    m = re.match(r"^(\s*)(.*?)((?:==|!=)\s+.*)$", line)
    if m:
        indent, prefix, rest = m.group(1), m.group(2), m.group(3)
        prefix = prefix.rstrip()
        if prefix:
            return f"{indent}{prefix} {annotation} {rest}"
        else:
            return f"{indent}{annotation} {rest}"
    return annotation + " " + line


def reftest_list_fuzzy(test_path, max_diff, total_pixels, platform, repo_root):
    test_filename = os.path.basename(test_path.lstrip("@"))
    list_path = find_reftest_list(test_path, repo_root)

    if not os.path.exists(list_path):
        print(f"Error: Could not find reftest.list for {test_path}", file=sys.stderr)
        return 1

    with open(list_path) as f:
        lines = f.readlines()

    idx, line = find_test_line(lines, test_filename)
    if idx is None:
        print(
            f"Error: Test '{test_filename}' not found in {list_path}", file=sys.stderr
        )
        return 1

    if platform:
        annotation = f"fuzzy-if({platform},{max_diff},{total_pixels})"
        pat = re.compile(r"fuzzy-if\(" + re.escape(platform) + r",[^)]*\)\s*")
        line = pat.sub("", line)
    else:
        annotation = f"fuzzy({max_diff},{total_pixels})"
        line = re.sub(r"(?<!-)fuzzy\([^)]*\)\s*", "", line)

    line = add_annotation_to_line(line, annotation)
    lines[idx] = line if line.endswith("\n") else line + "\n"

    with open(list_path, "w") as f:
        f.writelines(lines)

    print(f"Updated {os.path.relpath(list_path, repo_root)} line {idx + 1}")


def reftest_list_expect(test_path, result, platform, repo_root):
    test_filename = os.path.basename(test_path.lstrip("@"))
    list_path = find_reftest_list(test_path, repo_root)

    if not os.path.exists(list_path):
        print(f"Error: Could not find reftest.list for {test_path}", file=sys.stderr)
        return 1

    with open(list_path) as f:
        lines = f.readlines()

    idx, line = find_test_line(lines, test_filename)
    if idx is None:
        print(
            f"Error: Test '{test_filename}' not found in {list_path}", file=sys.stderr
        )
        return 1

    result = result.upper()

    annotation_map = {
        "FAIL": ("fails", "fails-if"),
        "RANDOM": ("random", "random-if"),
    }

    if result not in annotation_map:
        print(
            f"Error: For reftest.list, expected result must be FAIL or RANDOM, got '{result}'",
            file=sys.stderr,
        )
        print(
            "(reftest.list uses 'fails'/'fails-if' and 'random'/'random-if' annotations)",
            file=sys.stderr,
        )
        return 1

    bare, conditional = annotation_map[result]

    if platform:
        annotation = f"{conditional}({platform})"
        pat = re.compile(re.escape(conditional) + r"\(" + re.escape(platform) + r"\)\s*")
        line = pat.sub("", line)
    else:
        annotation = bare
        line = re.sub(r"\b" + re.escape(bare) + r"\b\s+", "", line)
        line = re.sub(re.escape(conditional) + r"\([^)]*\)\s*", "", line)

    line = add_annotation_to_line(line, annotation)
    lines[idx] = line if line.endswith("\n") else line + "\n"

    with open(list_path, "w") as f:
        f.writelines(lines)

    print(f"Updated {os.path.relpath(list_path, repo_root)} line {idx + 1}")


def reftest_list_disable(test_path, bug_url, platform, repo_root):
    test_filename = os.path.basename(test_path.lstrip("@"))
    list_path = find_reftest_list(test_path, repo_root)

    if not os.path.exists(list_path):
        print(f"Error: Could not find reftest.list for {test_path}", file=sys.stderr)
        return 1

    with open(list_path) as f:
        lines = f.readlines()

    idx, line = find_test_line(lines, test_filename)
    if idx is None:
        print(
            f"Error: Test '{test_filename}' not found in {list_path}", file=sys.stderr
        )
        return 1

    bug_comment = f" # {bug_url}" if bug_url else ""

    if platform:
        annotation = f"skip-if({platform})"
        pat = re.compile(r"skip-if\(" + re.escape(platform) + r"\)\s*")
        line = pat.sub("", line)
    else:
        annotation = "skip"
        line = re.sub(r"\bskip\b\s+", "", line)
        line = re.sub(r"skip-if\([^)]*\)\s*", "", line)

    line = line.rstrip("\n")
    line = add_annotation_to_line(line, annotation)

    if bug_comment and bug_url not in line:
        existing_comment = re.search(r"\s*#.*$", line)
        if existing_comment:
            line = line + f" ({bug_url})"
        else:
            line = line + bug_comment

    lines[idx] = line if line.endswith("\n") else line + "\n"

    with open(list_path, "w") as f:
        f.writelines(lines)

    print(f"Updated {os.path.relpath(list_path, repo_root)} line {idx + 1}")


# ---------------------------------------------------------------------------
# Mach command definitions
# ---------------------------------------------------------------------------


@Command(
    "test-expectations",
    category="testing",
    description="Manage reftest expectation files (.ini for WPT, reftest.list for others)",
)
def test_expectations(command_context):
    pass


@SubCommand(
    "test-expectations",
    "fuzzy",
    description="Add or update fuzzy annotation for a test",
)
@CommandArgument("test_path", help="Path to the test")
@CommandArgument("max_diff", help="maxDifference range, e.g. 0-1")
@CommandArgument("total_pixels", help="totalPixels range, e.g. 0-500")
@CommandArgument("--platform", default=None, help="Platform condition")
def test_expectations_fuzzy(
    command_context, test_path, max_diff, total_pixels, platform=None
):
    repo_root = command_context.topsrcdir
    if is_wpt_test(test_path):
        wpt_fuzzy(test_path, max_diff, total_pixels, platform, repo_root)
    else:
        return reftest_list_fuzzy(test_path, max_diff, total_pixels, platform, repo_root)


@SubCommand(
    "test-expectations",
    "expect",
    description="Set expected result for a test",
)
@CommandArgument("test_path", help="Path to the test")
@CommandArgument("result", help="Expected result: FAIL, PASS, TIMEOUT, ERROR, RANDOM, etc.")
@CommandArgument("--subtest", default=None, help="Subtest name (WPT only)")
@CommandArgument("--platform", default=None, help="Platform condition")
def test_expectations_expect(
    command_context, test_path, result, subtest=None, platform=None
):
    repo_root = command_context.topsrcdir
    if is_wpt_test(test_path):
        wpt_expect(test_path, result, subtest, platform, repo_root)
    else:
        return reftest_list_expect(test_path, result, platform, repo_root)


@SubCommand(
    "test-expectations",
    "disable",
    description="Disable a test with a bug reference",
)
@CommandArgument("test_path", help="Path to the test")
@CommandArgument("bug_url", help="Bugzilla bug URL")
@CommandArgument("--platform", default=None, help="Platform condition")
def test_expectations_disable(command_context, test_path, bug_url, platform=None):
    repo_root = command_context.topsrcdir
    if is_wpt_test(test_path):
        wpt_disable(test_path, bug_url, platform, repo_root)
    else:
        return reftest_list_disable(test_path, bug_url, platform, repo_root)
