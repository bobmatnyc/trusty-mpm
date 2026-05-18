# Trusty Tool Priority

You are running inside trusty-mpm. The following MCP tools are available and MUST be preferred over alternatives:

## Memory: trusty-memory (use BEFORE any other memory mechanism)
- `memory_recall` — semantic + temporal search across your memory palace. Use this FIRST whenever you need to recall context, prior decisions, or project knowledge.
- `memory_recall_deep` — deeper HNSW search when `memory_recall` returns insufficient results.
- `memory_remember` — store important decisions, findings, and facts immediately after they arise. Do not defer.
- `memory_list` — list stored memories by room or tag.
- `memory_forget` — remove outdated or incorrect memories.
- `palace_list` / `palace_create` — manage named memory palaces (one per project is typical).

Always call `memory_recall` at the start of any task to surface relevant prior context before taking action.

## Code Search: trusty-search (use BEFORE grep or web search for code questions)
- `search_code` — hybrid BM25 + vector + knowledge graph search. Use this FIRST for any "where is X defined", "how does Y work", or "find all usages of Z" questions.
- `search_all` — cross-project search when the target may span multiple codebases.
- `search_similar` — find semantically similar code chunks to a given file or function.
- `search_health` — verify the search daemon is live before a search session.

Always prefer `trusty-search` over shell `grep`/`find` for code discovery. Use grep only for exact-string or regex patterns that semantic search cannot handle.

## Priority order for common tasks
1. **Recall context** → `memory_recall` first
2. **Find code** → `search_code` before grep
3. **Store findings** → `memory_remember` after significant discoveries
4. **Cross-project** → `search_all` when scope is unclear
