# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo build                   # debug build
cargo build --release         # release build
cargo run                     # run locally on port 3000
cargo check                   # fast type/borrow check without building
cargo clippy                  # lint
```

For the static Linux binary (used in CI/CD):
```bash
cargo zigbuild --target x86_64-unknown-linux-musl --release
```

No tests exist in this project.

## Architecture

Single-crate Rust web service. All code lives in `src/main.rs` (~769 lines). No modules.

**Stack:** Axum 0.7 + Tokio async runtime, port 3000, CORS enabled via tower-http.

**API endpoints:**
- `POST /scrape` — single page extraction
- `POST /scrape-all` — multi-chapter extraction (follows TOC links)
- `GET /health` — health check

**Request:** `{"url": "https://..."}`
**Response:** `{"success": bool, "html": "...", "title": "...", "error": "..."}`
Multi-chapter also returns `total` and `titles[]`.

**Data pipeline (both endpoints):**
1. `fetch_html` — HTTP GET with encoding detection (GBK/GB2312 → UTF-8 via `encoding_rs`)
2. `parse_html` — CSS selector-based extraction of title + body content using `scraper`
3. `collect_and_download_images` — concurrent image download, convert to Base64 data URIs
4. `apply_inline_styles` — inject inline CSS on all elements for rich text editor compatibility
5. `div_to_p` — normalize `<div>` → `<p>` for editor support
6. Return assembled HTML fragment

For `/scrape-all`: `extract_chapter_links` parses a TOC page first, then runs the pipeline on each chapter URL sequentially, concatenating results.

**Key design notes:**
- Images are embedded as Base64 data URIs so the output is self-contained
- Inline styles are merged (not replaced) to preserve existing styles
- Relative URLs are resolved to absolute before processing
- The output is intended for pasting into rich text editors, not for browser rendering
