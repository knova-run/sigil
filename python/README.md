# sigil-diff

Python bindings for [sigil](https://github.com/knova-run/sigil) — structural code fingerprinting and diffing.

```
pip install sigil-diff
```

```python
import sigil
```

Powered by Rust via PyO3. Native speed, no subprocess overhead, works with in-memory strings.

## Quick Start

```python
import sigil

old = '{"body": {"text": "Hello world"}, "header": {"text": ""}}'
new = '{"body": {"text": "Hello universe"}, "header": {"text": ""}}'

result = sigil.diff_json(old, new)

for f in result["files"]:
    for entity in f["entities"]:
        print(f"{entity['change']}: {entity['name']}")
        for tc in entity.get("token_changes", []):
            print(f"  \"{tc['from']}\" -> \"{tc['to']}\"")
```

Output:

```
modified: body.text
  ""text": "Hello world"," -> ""text": "Hello universe","
```

## API

### `sigil.diff_json(old: str, new: str) -> dict`

Diff two JSON strings. Returns a structured dict with all changes.

This is the primary function for comparing JSON documents (notification templates, config files, API responses, etc.). Minified JSON is automatically formatted before diffing.

```python
result = sigil.diff_json(old_json_str, new_json_str)
```

**Features applied automatically:**
- Parent-aware matching (no cross-matching between `body.text` and `header.text`)
- Derived field suppression (`_`-prefixed fields hidden from output)
- Array item identity matching (objects matched by `id`/`key`/`name`/`text`/`type`)
- Qualified names (`body.text` instead of bare `text`)
- Parent entity suppression (when children carry the detail)

### `sigil.diff_files(old_path: str, new_path: str) -> dict`

Diff two files on disk. Supports JSON, YAML, TOML, Markdown, Python, Rust, Go, JavaScript, TypeScript, Java, Ruby, C, C++, C#, Swift, Kotlin.

```python
result = sigil.diff_files("templates/old.json", "templates/new.json")
```

### `sigil.diff_refs(repo_path: str, base_ref: str, head_ref: str) -> dict`

Diff two git refs in a repository. Works with any ref format: commit SHAs, branch names, tags, `HEAD~N`.

```python
result = sigil.diff_refs("/path/to/repo", "main", "feature-branch")
result = sigil.diff_refs(".", "HEAD~1", "HEAD")
```

### `sigil.index_json(source: str) -> list[dict]`

Parse a JSON string into structural entities. Returns a list of entity dicts.

```python
entities = sigil.index_json('{"name": "test", "tags": ["a", "b"], "_internal": "hidden"}')

for e in entities:
    derived = " (derived)" if e.get("meta") and "derived" in e["meta"] else ""
    parent = f" -> {e['parent']}" if e.get("parent") else ""
    print(f"{e['name']}: {e['kind']}{parent}{derived}")
```

Output:

```
name: property
tags: array
[0]: element -> tags
[1]: element -> tags
_internal: property (derived)
```

## Return Format

All diff functions return a dict with this structure:

```python
{
    "meta": {
        "base_ref": "old",
        "head_ref": "new",
        "sigil_version": "0.2.4"
    },
    "summary": {
        "files_changed": 1,
        "added": 0,
        "removed": 0,
        "modified": 1,
        "renamed": 0,
        "has_breaking": False,
        "natural_language": "1 modified."
    },
    "files": [
        {
            "file": "doc.json",
            "summary": {"added": 0, "modified": 1, "removed": 0, ...},
            "entities": [
                {
                    "change": "modified",       # added | removed | modified | renamed | formatting_only
                    "name": "body.text",         # qualified name for JSON entities
                    "kind": "property",          # property | object | array | element | function | class | ...
                    "line": 3,
                    "line_end": 3,
                    "sig_changed": False,        # signature (key name/type) changed?
                    "body_changed": True,         # value changed?
                    "breaking": False,
                    "token_changes": [
                        {
                            "type": "value_changed",
                            "from": "Hello world",
                            "to": "Hello universe"
                        }
                    ],
                    "context": {                  # source snippets for display
                        "hunks": [
                            {"kind": "removed", "text": "..."},
                            {"kind": "added", "text": "..."}
                        ]
                    }
                }
            ]
        }
    ],
    "breaking": [],       # list of breaking change entries
    "patterns": [],       # cross-file patterns (e.g., same rename across files)
    "moves": []           # entities moved between files
}
```

## Examples

### Compare notification templates

```python
import sigil
import json

old_template = json.dumps({
    "body": {
        "text": "Hi {{user.name}}, your order #{{order_id}} is confirmed.",
        "_parsed_text": "Hi {{1}}, your order #{{2}} is confirmed.",
        "_examples": ["John", "12345"]
    },
    "buttons": [
        {"text": "Track Order", "type": "URL"}
    ]
})

new_template = json.dumps({
    "body": {
        "text": "Hi {{user.name}}, order #{{order_id}} is on its way!",
        "_parsed_text": "Hi {{1}}, order #{{2}} is on its way!",
        "_examples": ["John", "12345"]
    },
    "buttons": [
        {"text": "Track Order", "type": "URL"}
    ]
})

result = sigil.diff_json(old_template, new_template)

if result["summary"]["modified"] > 0:
    for f in result["files"]:
        for e in f["entities"]:
            print(f"Changed: {e['name']}")
            for tc in e.get("token_changes", []):
                print(f"  {tc['from']}")
                print(f"  {tc['to']}")
```

### Check if two documents are identical

```python
result = sigil.diff_json(old_str, new_str)
is_identical = result["summary"]["modified"] == 0 and result["summary"]["added"] == 0 and result["summary"]["removed"] == 0
```

### Get changed field names

```python
result = sigil.diff_json(old_str, new_str)
changed_fields = [
    e["name"]
    for f in result["files"]
    for e in f["entities"]
]
```

## Development

```bash
# Clone the repo
git clone https://github.com/knova-run/sigil.git
cd sigil/python

# Create venv and build
python3 -m venv .venv
source .venv/bin/activate
pip install maturin
maturin develop

# Test
python3 -c "import sigil; print(sigil.diff_json('{\"a\":1}', '{\"a\":2}'))"
```

### Build a wheel

```bash
maturin build --release
# Output: target/wheels/sigil_diff-0.2.4-cp3XX-*.whl
```
