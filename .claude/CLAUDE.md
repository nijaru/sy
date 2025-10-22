# AI Development Context for sy

**Note**: This file is maintained for legacy compatibility. The current AI entry point is `AGENTS.md` in the project root.

## Quick Start

Load the main AI context:
```
@AGENTS.md
```

## Organization Structure

The sy project follows standardized AI agent organization patterns:

- **AGENTS.md** - Main AI entry point (project context, structure, conventions)
- **ai/** - AI working context
  - `TODO.md` - Active tasks and priorities
  - `STATUS.md` - Current project state, what worked/didn't
  - `DECISIONS.md` - Architectural decisions with rationale
  - `RESEARCH.md` - External research findings index
- **docs/** - Project documentation (user and developer facing)
- **src/** - Rust source code
- **tests/** - Integration tests
- **benches/** - Performance benchmarks

## For New Sessions

1. Load `@AGENTS.md` for project overview and structure
2. Check `ai/TODO.md` for active work
3. Check `ai/STATUS.md` for current state
4. Reference `ai/DECISIONS.md` for architectural context
5. See `DESIGN.md` for comprehensive technical design

## Organization Patterns

This project follows the patterns defined in [@external/agent-contexts/PRACTICES.md](https://github.com/nijaru/agent-contexts):

- **ai/** directory for agent working context (not project docs)
- **docs/** directory for project documentation
- **AGENTS.md** as the AI entry point
- Separation of agent context from user-facing documentation

---

**For complete context, load**: `@AGENTS.md`
