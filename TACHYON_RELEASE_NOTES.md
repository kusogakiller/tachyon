# Tachyon 25.7.1-tachyon.1 — Release Notes

Tachyon is a semantic editing layer for Helix built on one algebra:

    TARGET × COUNT × DIRECTION × ACTION × REPEAT   (always against the CURRENT tree)

    t [count] [direction] <target> <action>

## Highlights

- `t f d`            delete the current function
- `t 3 f d`          delete three functions
- `t -2 f y`         yank the two previous functions
- `t -2 n d .`       delete previous parameters with separator repair, repeat on the current tree
- `t 2 s c`          counted Change through Helix's native multi-cursor insert
- `t ?`              prefix-aware Explorer — discover the grammar without memorizing keys

No Vim equivalent exists for the combination packed into `t -2 n d`: semantic
parameter selection + backward traversal + count + delimiter repair +
multiline/CRLF correctness + current-tree repeat + transaction-safe multi-range
application. The same grammar works identically across Rust, Python, Go, and C
tree-sitter structures.

## Targets

| Target    | Mechanism                          | Status              |
|-----------|------------------------------------|---------------------|
| Function  | tree-sitter `@function.inside`     | verified            |
| Class     | `@class.inside`                    | verified            |
| Parameter | `@parameter.inside`                | deeply verified     |
| Argument  | text pair-surround                 | fallback by design  |
| Statement / Expression / Block | no bundled captures | fallback by design |
| Word/Line/Paragraph/String/Brackets/All | text primitives | supported |

## Actions

Delete · Yank · Change · Indent · Outdent — all support single / counted /
backward / multi-range application through ONE execution funnel
(`apply_target_selection`), with repeat re-resolving against the current
document. Persistent state holds intent only:
`(Target, Action, count, Direction, MultiSelectIntent)` — never geometry.

## Verification

- workspace tests green (helix-core 180, helix-term 243 incl. 122 target tests)
- real-tree harness over four bundled grammars (Rust, Python, Go, C)
- real-dispatch integration smoke tests: Explorer flow, backward counted
  parameter delete + repeat, counted multi-cursor Change
- fallback contract pinned: unsupported captures can never fabricate ranges

## Notes

- One upstream Helix integration test (`select_all_siblings`) has been
  observed to fail intermittently under parallel load; bisected as
  independent of Tachyon and passing in the final release run.
- Base: Helix 25.7.1.
