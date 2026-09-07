# Third-party notices

This document lists third-party assets bundled inside the `claude-usage`
binary. Source dependency licenses are collected by `cargo`; this file covers
only assets that ship as bytes inside the binary itself.

## Menu-bar provider icons

The 16px PNG icons in `assets/icons/16/` are decorations shown next to each
provider's section header in the macOS menu bar. They are compiled into the
binary via `include_bytes!` in `src/icons.rs`.

Sources and licenses:

| File                                | Source                                        | License   | Upstream slug     |
|-------------------------------------|-----------------------------------------------|-----------|-------------------|
| `assets/icons/16/claude-code.png`   | Simple Icons — https://simpleicons.org        | CC0-1.0   | `claude`          |
| `assets/icons/16/codex.png`         | Lobe icons — https://github.com/lobehub/lobe-icons | MIT  | `openai`          |
| `assets/icons/16/opencode.png`      | Simple Icons — https://simpleicons.org        | CC0-1.0   | `opencode`        |
| `assets/icons/16/gemini-cli.png`    | Lobe icons — https://github.com/lobehub/lobe-icons | MIT  | `gemini`          |
| `assets/icons/16/qwen-code.png`     | Simple Icons — https://simpleicons.org        | CC0-1.0   | `qwen`            |
| `assets/icons/16/copilot-cli.png`   | Simple Icons — https://simpleicons.org        | CC0-1.0   | `githubcopilot`   |
| `assets/icons/16/cursor-agent.png`  | Simple Icons — https://simpleicons.org        | CC0-1.0   | `cursor`          |
| `assets/icons/16/amazon-q.png`      | Lobe icons — https://github.com/lobehub/lobe-icons | MIT  | `aws`             |
| `assets/icons/16/cline.png`         | Simple Icons — https://simpleicons.org        | CC0-1.0   | `cline`           |
| `assets/icons/16/grok.png`          | Lobe icons — https://github.com/lobehub/lobe-icons | MIT  | `xai`             |
| `assets/icons/16/kimi.png`          | Simple Icons — https://simpleicons.org        | CC0-1.0   | `kimi`            |
| `assets/icons/16/openrouter.png`    | Simple Icons — https://simpleicons.org        | CC0-1.0   | `openrouter`      |
| `assets/icons/16/deepseek.png`      | Simple Icons — https://simpleicons.org        | CC0-1.0   | `deepseek`        |
| `assets/icons/16/zai.png`           | Lobe icons — https://github.com/lobehub/lobe-icons | MIT  | `zai`             |
| `assets/icons/16/fireworks.png`     | Lobe icons — https://github.com/lobehub/lobe-icons | MIT  | `fireworks`       |
| `assets/icons/16/synthetic.png`     | Monogram placeholder (generated in-house)     | n/a       | —                 |

### License texts

**Simple Icons — CC0 1.0 Universal (Public Domain Dedication).** The person
who associated a work with this deed has dedicated the work to the public
domain by waiving all of his or her rights to the work worldwide under
copyright law, including all related and neighboring rights, to the extent
allowed by law. See https://creativecommons.org/publicdomain/zero/1.0/.

**Lobe icons — MIT License.** Copyright (c) LobeHub, Inc. Permission is hereby
granted, free of charge, to any person obtaining a copy of this software and
associated documentation files (the "Software"), to deal in the Software
without restriction, including without limitation the rights to use, copy,
modify, merge, publish, distribute, sublicense, and/or sell copies of the
Software, and to permit persons to whom the Software is furnished to do so,
subject to the following conditions: the above copyright notice and this
permission notice shall be included in all copies or substantial portions of
the Software. THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND.
See https://github.com/lobehub/lobe-icons for the full text.
