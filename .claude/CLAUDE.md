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
  - `STATUS.md` - Current project state, what worked/didn't
  - `TODO.md` - Active tasks and priorities
  - `DECISIONS.md` - Architectural decisions with rationale
  - `RESEARCH.md` - Research findings index
  - `research/` - Research documents
- **README.md** - User-facing documentation
- **CONTRIBUTING.md** - Development guidelines
- **src/** - Rust source code
- **tests/** - Integration tests
- **benches/** - Performance benchmarks

## For New Sessions

1. Load `@AGENTS.md` for project overview and structure
2. Check `ai/STATUS.md` for current state
3. Check `ai/TODO.md` for active work
4. Reference `ai/DECISIONS.md` for architectural context

## Organization Patterns

This project follows the patterns defined in [@external/agent-contexts/PRACTICES.md](https://github.com/nijaru/agent-contexts):

- **ai/** directory for agent working context
- **AGENTS.md** as the AI entry point
- Minimal, maintainable documentation (README, CONTRIBUTING, CHANGELOG only)

## Release Versioning Strategy

**CRITICAL: sy is a file sync tool - data safety is paramount.**

### Version Numbers (0.0.x for now)

**0.0.x** (Current) - "Works great in testing, use at your own risk"
- For: Early adopters, testing, non-critical data
- Signals: Well-tested but not battle-proven
- Continue: Until 3-6 months of real-world usage without data loss

**0.1.0** (Future) - "Production-ready, proven in the wild"
- Need first: Months of 0.0.x releases with diverse real users
- Requires:
  - No data loss reports
  - User testimonials from production use
  - Major bugs shaken out
  - Proven across different environments
- Signals: API stabilizing, safe for production

**1.0.0** (Distant future) - "Stable, widely trusted, battle-tested"
- Years away (like rsync's maturity)
- Only after 0.x series proves itself in production

### Why This Matters

File sync tools that lose data destroy trust forever. No amount of testing replaces real-world usage:
- ✅ Tests show what we checked
- ❌ Tests can't predict every environment
- ❌ Edge cases emerge from actual use

**Never jump to 0.1.0 or 1.0.0 based on test results alone.** Wait for production validation.

### When Planning Releases

- Add features/fixes → bump 0.0.x (0.0.48 → 0.0.49)
- Don't suggest 0.1.0 unless user explicitly has production proof
- Protect users and reputation by staying conservative

---

**For complete context, load**: `@AGENTS.md`
