---
name: patchwork
description: AST-native code transformation — use patchwork CLI instead of sed/regex for structural find/replace/delete/insert operations
---

# patchwork

AST-native sed. Find, replace, delete, and insert code by structure, not regex.

## When to use

Use patchwork instead of sed/grep/regex when:
- Renaming functions/methods/variables across files
- Replacing API calls with different signatures
- Deleting logging/debug statements
- Matching code patterns that span multiple lines
- You need structural accuracy (avoid false positives from string matching)

Do NOT use for: simple string replacements, non-code files, languages not supported.

## Supported languages

Java, Python, JavaScript, TypeScript, TSX, Rust, Go, Ruby, C, C++, C#, PHP, Bash

## Commands

| Command | Effect |
|---------|--------|
| `find` | Print `file:line:col` for each match |
| `replace` | Replace matched code with new code |
| `delete` | Remove matched code |
| `insert-before` | Insert code before each match |
| `insert-after` | Insert code after each match |
| `nodes` | List available tree-sitter node kinds for a language |

## Pattern syntax

### Basic placeholders

- `$name` — matches any single AST node (identifier, literal, expression)
- `$name:Kind` — matches only nodes of specific tree-sitter kind (e.g., `$x:identifier`, `$n:string_literal`)

### Special tokens

| Token | Matches |
|-------|---------|
| `$BODY` | Zero or more statements inside a block |
| `$STMT` | A single statement of any kind |
| `$EXPR` | A single expression |

### Repetition syntax

- `$($name)sep*` — Rust-style repetition with separator awareness (zero or more)
- `$($name)sep+` — one or more
- `$($a, $b)sep*` — multi-item repetition groups
- `$$$name` — position-independent catch-all (no separator tracking)

## Examples

```bash
# Rename method across files
patchwork replace -i -p 'getOldData($a)' -r 'getData($a)' src/**/*.java

# Delete all debug calls
patchwork delete -i -p 'debug($msg)' src/*.py

# Find return statements
patchwork find -p 'return $val;' src/

# Match function calls with any number of args (separator-aware)
patchwork find -p '$f($($arg,)*);' src/

# Find all if statements
patchwork find -p 'if ($EXPR) { $BODY }' src/

# Replace with arg reordering
patchwork replace -i -p '$f($a, $b)' -r '$f($b, $a)' src/*.py

# Match only identifiers (not property access) on LHS
patchwork find -p '$x:identifier = $val;' src/

# List node kinds for type constraints
patchwork nodes java
patchwork nodes python --all  # include punctuation/operators
```

## Flags

- `-i, --in-place` — modify files directly (like `sed -i`)
- `-l, --language` — force language detection
- `-p, --pattern` — code snippet pattern
- `-q, --query` — tree-sitter query (advanced)
- `-r, --replacement` — replacement code (for `replace`)
- `--code` — code to insert (for `insert-before`/`insert-after`)

## Pipe mode

```bash
cat Main.java | patchwork find -l java -p 'return null;'
```

## Limitations

- Single-file only — no cross-file rename tracking or import updates
- Replacement text isn't auto-indented
- `$$$name` doesn't match statements inside blocks (use `$BODY`)
