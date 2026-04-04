---
name: review-pr
description: Review a GitHub pull request with actionable feedback
allowed-tools:
  - Bash(gh:*)
  - Read
  - Grep
model: opus
when_to_use: Use when the user asks to review a PR, check a pull request, or give feedback on code changes
argument-hint: "<pr-number>"
---
# Review PR Skill

Perform a thorough code review of a GitHub pull request.

## Inputs
- `$ARGUMENTS`: PR number (e.g., `123`) or URL

## Steps

### 1. Fetch PR metadata
```bash
gh pr view $ARGUMENTS --json title,body,files,additions,deletions
```

### 2. Read the diff
```bash
gh pr diff $ARGUMENTS
```

### 3. Review checklist
For each changed file, evaluate:
- [ ] Correctness: Does the logic do what it claims?
- [ ] Security: Any injection, auth, or data exposure risks?
- [ ] Performance: O(n²) loops, missing indexes, unnecessary allocations?
- [ ] Tests: Are new code paths covered?
- [ ] Style: Consistent with surrounding code?

### 4. Provide feedback
Structure your review as:
1. **Summary**: One-line assessment (approve / request changes)
2. **Highlights**: What's done well
3. **Issues**: Specific file:line references with suggested fixes
4. **Questions**: Anything unclear about intent

**Success criteria**: Actionable review with specific file:line references.
