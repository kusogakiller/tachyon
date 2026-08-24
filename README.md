# Tachyon

Tachyon is a semantic editing layer derived from the [Helix Editor](https://github.com/helix-editor/helix)
codebase. It keeps Helix's kernel — modal editing, multiple selections,
tree-sitter integration, built-in language-server support — and adds a
target-first semantic grammar on top of it:

    t [count] [direction] <target> <action>

The model is one algebra:

    TARGET × COUNT × DIRECTION × ACTION × REPEAT × CURRENT TREE

Every operation resolves against the tree-sitter parse of the **current**
document. Persistent repeat state holds intent only — never selections,
ranges, positions, or tree-sitter nodes.

## Examples

```text
t ?        open the prefix-aware Explorer (discover targets/actions live)
t f d      delete the current function
t 3 f d    delete three functions
t -2 f y   yank the two previous functions
t -n d     delete the previous parameter (separator repaired)
t -2 n d . delete the previous two parameters, then repeat on the current tree
t 2 s c    counted Change through Helix's native multi-cursor insert
```

`t -2 n d` combines semantic parameter selection, backward traversal, count,
delimiter repair, multiline/CRLF correctness, current-tree semantics and
transaction-safe multi-range application — a composition with no direct Vim
operator equivalent. Because resolution is grammar-driven, the same commands
work identically across Rust, Python, Go, C, and other languages whose
tree-sitter queries expose the standard captures.

## Semantics

- **Count** multiplies semantic scope (`t 3 f d`). Count `0` is rejected.
- **Direction**: leading `-` selects backward traversal (`t -3 f d`); results
  stay document-ordered.
- **Repeat** (`.`) replays `(Target, Action, count, Direction)` intent against
  the **current** document — mutation between repeats is honored, stale
  geometry is never replayed.
- **Counted Change** clears N targets and opens one multi-cursor insert
  session (one cursor per target).
- **Delimiter-aware Parameter deletion** repairs surrounding separators, is
  multiline-safe, and handles both LF and CRLF line endings.

## Targets

| Target | Resolution |
|---|---|
| `f` function · `g` class · `n` parameter | tree-sitter captures (`@function.inside`, `@class.inside`, `@parameter.inside`) |
| `e` expression · `s` statement · `b` block | documented fallback when the grammar exposes no capture — never fabricated |
| `(` argument · `"` string · `%` brackets · `w` word · `l` line · `p` paragraph · `a` all | text primitives |

Actions: `d` delete · `c` change · `y` yank · `>` indent · `<` outdent.

Press `t ?` at any time to browse every target and action with the active
prefix displayed.

## Building

```sh
cargo install --path helix-term --locked   # installs the `hx` binary
```

Requires Rust 1.90.0 (see `rust-toolchain.toml`). Grammars are compiled on
first run.

## Testing

```sh
cargo test --workspace
cargo test -p helix-term --features integration --test integration tachyon
```

Release `v25.7.1-tachyon.1` ships with a green workspace run, real-tree
verification across four bundled grammars (Rust, Python, Go, C), and
real-dispatch integration smoke tests covering the Explorer flow, backward
counted parameter deletion with repeat, and counted Change.

## Project status

Semantic core is released as `v25.7.1-tachyon.1`. Documentation under
`docs/` and `book/` describes the inherited Helix base and remains the
reference for editor features outside the Tachyon semantic layer.

## Origin / Attribution

Tachyon is derived from the Helix Editor codebase:
[helix-editor/helix](https://github.com/helix-editor/helix).

Helix is licensed under the Mozilla Public License 2.0; see
[LICENSE](./LICENSE). All upstream copyright holders and license obligations
remain intact. Tachyon is an independent project and is **not** affiliated
with, endorsed by, or maintained by the Helix project or its contributors.
