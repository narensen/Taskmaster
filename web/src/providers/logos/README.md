# Provider marks

One SVG per harness id, named after the id the daemon uses (`tm-harness::HARNESSES`):

| file           | provider            | mark                    |
| -------------- | ------------------- | ----------------------- |
| `claude.svg`   | Claude Code         | Anthropic's Claude mark |
| `grok.svg`     | Grok Build          | xAI's Grok mark         |
| `codex.svg`    | Codex               | OpenAI's Codex mark     |
| `opencode.svg` | OpenCode            | OpenCode's mark         |
| `agy.svg`      | Antigravity         | Google's Antigravity mark |

The single-colour paths come from LobeHub's `@lobehub/icons-static-svg` (MIT), which
redraws each brand's published mark as one `currentColor` shape. The marks are the
trademarks of their owners and are used only to say which provider is in use.

`../logos.tsx` draws them in the theme's ink, with no tile or background; a brand colour
appears only on hover. A provider without a file here gets a plain ring with its initial.
To add one, drop `<id>.svg` (a 24×24 viewBox, `fill="currentColor"`) in this folder and
add it to `MARKS` in `../logos.tsx`.
