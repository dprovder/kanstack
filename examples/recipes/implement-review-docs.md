---
version: 1

steps:
  implement:
    agent: codex
    model: gpt-5.6
    effort: high
    prompt: Add a tiny harmless change requested by the test issue.

  review:
    agent: claude
    effort: high
    needs: [implement]
    on: implement
    prompt: Review the implementation and fix any issue you find.
    verify:
      - cargo test

  docs:
    agent: codex
    effort: low
    needs: [review]
    on: implement
    prompt: Update documentation if the change requires it.
---

# Context

Keep the patch minimal and preserve existing behavior.
