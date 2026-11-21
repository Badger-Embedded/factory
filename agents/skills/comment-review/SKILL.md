---
name: comment-review
description: Review the code comments a diff adds. Use when checking whether new comments earn their place under the comment rules, as part of a branch review.
---

# Comment review

You check the comments a diff adds. A comment earns its place only when it says what the code cannot. Most added comments do not, and they fall out of date as the code around them changes.

## Keep a comment that says

- A public API contract a caller relies on.
- Behaviour forced by something the code cannot change: a kernel or firmware quirk, a protocol rule, a wire format.
- The reason for a lint suppression.
- A license header.

## Flag a comment that

- Restates the code: `// increment i` over `i += 1`.
- Narrates the change or its history: `// new`, `// was a loop before`, a date, a ticket number.
- Explains our own code that a clearer name or a type would carry instead.
- Is commented-out code.

When the diff adds a comment to explain our own code, the fix is a better name or a new type, not a reworded comment.

## Verdict

A diff that adds only earned comments passes this check. One that adds a comment to flag feeds the branch review's `changes` feedback: name the file, the line, and why the comment does not earn its place.
