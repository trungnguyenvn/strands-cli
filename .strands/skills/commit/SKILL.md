---
name: commit
description: Create a well-formatted git commit with conventional commit style
allowed-tools:
  - Bash(git:*)
  - Read
when_to_use: Use when the user asks to commit changes, create a commit, or save their work
argument-hint: "[-m 'message']"
---
# Commit Skill

Create a git commit following conventional commit conventions.

## Steps

### 1. Check status
Run `git status` and `git diff --staged` to understand what's being committed.

### 2. Analyze changes
Read the staged changes to understand the nature of the modification:
- Is this a new feature (`feat:`)?
- A bug fix (`fix:`)?
- Refactoring (`refactor:`)?
- Documentation (`docs:`)?
- Tests (`test:`)?

### 3. Draft commit message
Write a concise commit message:
- Subject line: `<type>: <description>` (max 72 chars)
- Body: explain **why**, not what (the diff shows what)

### 4. Create the commit
```bash
git commit -m "<message>"
```

**Success criteria**: Commit created with a clear, conventional message.
