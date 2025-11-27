# Research: Tilde Expansion in Remote Paths

**Context**: `sy --server` mode failed to sync files to `~/path` on remote host. Directories were not created, or created in wrong location.

**Problem**:
- `sy` client sends `ssh user@host sy --server ~/path`.
- `tokio::process::Command` executes this.
- If SSH executes command via shell, `~` is expanded.
- If SSH executes directly (exec), `~` is literal.
- Rust `std::fs` does not handle `~`.

**Investigation**:
- `Command::new("ssh").arg("host").arg("cmd").arg("arg")`
- OpenSSH client generally joins args with spaces and sends to remote shell.
- However, if `sy` receives `~/path`, it passes it to `PathBuf`.
- `PathBuf` treats `~` as a directory name.
- `fs::create_dir_all("~/foo")` creates a directory named `~` in the current working directory.

**Verification**:
- Check if `~` directory exists in remote home dir (or CWD of SSH session).

**Solutions**:
1.  **Server-side expansion**:
    - `sy --server` entry point should detect leading `~` and replace with `$HOME`.
    - Use `dirs::home_dir()` on server side.
2.  **Client-side resolution**:
    - Client cannot resolve remote `~` easily (doesn't know remote user).
3.  **Shell invocation**:
    - Force shell usage: `ssh host "sy --server ~/path"`.
    - Quote issues?

**Decision**:
- Implement **Server-side expansion**. It's robust and handles the problem where it occurs.
- In `src/server/mod.rs`, before creating `ServerHandler`, expand `root_path`.

**Code Snippet**:
```rust
fn expand_home(path: PathBuf) -> PathBuf {
    if !path.starts_with("~") {
        return path;
    }
    if let Some(home) = dirs::home_dir() {
        // Naive replacement for "~/"
        // Or use specific logic
    }
}
```
Wait, `PathBuf` components.
If `path` is `~/foo`, components are `~`, `foo`.
If first component is `~`, replace with home dir.

**Reference**: `ai/STATUS.md` (Benchmark Failure)
