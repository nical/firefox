---
name: test-expectations
description: Update test expectation files (.ini for WPT, reftest.list for other reftests). Use when adding fuzzy annotations, expected results, disabling tests, or fixing test expectations.
argument-hint: "<test-path> [action: fuzzy|expect|disable] [details]"
model: haiku
allowed-tools:
  - Bash
---

# Reftest Expectations

Script: `python3 .claude/skills/test-expectations/test-expectations.py`

Auto-detects test type: WPT (`testing/web-platform/`) uses `.ini` files, others use `reftest.list`. Test path may start with `@` (stripped automatically). All commands accept `--platform='<condition>'` (WPT: `os == "win"`, reftest.list: `Android`, `geckoview`, `winWidget`).

## Commands

```bash
# Fuzzy: <test-path> <max-diff-range> <total-pixels-range>
python3 …/test-expectations.py fuzzy testing/web-platform/tests/css/foo/bar.html 0-1 0-500
python3 …/test-expectations.py fuzzy layout/reftests/svg/test.svg 0-5 0-254 --platform=Android

# Expect: <test-path> <result> [--subtest='<name>']
# WPT results: PASS, FAIL, TIMEOUT, ERROR, [PASS, FAIL]. Reftest: FAIL, RANDOM.
python3 …/test-expectations.py expect testing/web-platform/tests/css/foo/bar.html FAIL --subtest='my subtest'

# Disable: <test-path> <bug-url>
python3 …/test-expectations.py disable testing/web-platform/tests/css/foo/bar.html 'https://bugzilla.mozilla.org/show_bug.cgi?id=123456'
```
