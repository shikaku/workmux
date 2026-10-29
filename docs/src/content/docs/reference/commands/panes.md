---
title: "panes"
description: Open the panes of a layout that the current window lacks
---

Opens the panes of a layout that the current window does not have, in the places the layout gives them, and runs their commands. Panes already open, and what runs in them, are left as they are. tmux only.

```bash
workmux panes [layout] [-t <pane>] [--dry-run]
workmux panes --list
```

## Arguments

- `[layout]`: A name from [`layouts`](/guide/configuration/#named-layouts). `default`, or none, stands for the top-level `panes`.

## Options

| Flag              | Description                                                        |
| ----------------- | ------------------------------------------------------------------ |
| `--list`          | Print the layout names: `default`, then the `layouts` keys         |
| `-n, --dry-run`   | Say which panes would open, without opening them                   |
| `-t, --target`    | A pane of the window to work on (defaults to the current pane)     |

## How panes are matched

- The agent pane (`<agent>` and other agent commands) is the pane workmux tracks an agent in. It is never started: when it is missing, the command says so.
- Any other pane needs a `name`. It matches a pane that `workmux add` or `workmux panes` gave that name (the `@workmux_pane_name` pane option), or a pane whose title is that name. A pane without a name is skipped.

A missing pane is split from the pane the layout splits it from. When only a pane the layout split from it is open, the missing pane goes before that one (above it or to its left), sized so that pane keeps the size the layout gives it. Post-create hooks do not run again.

## Examples

```yaml
panes:
  - command: <agent>
    focus: true
  - command: npm run test:watch
    name: tests
    split: vertical
    size: 10
layouts:
  dev:
    panes:
      - command: npm run dev
        name: server
      - command: <agent>
        focus: true
        split: vertical
        percentage: 90
      - command: npm run test:watch
        name: tests
        split: vertical
        size: 10
```

```bash
# The window has the agent and tests: add the dev server above the agent
workmux panes dev

# Reopen a closed tests pane
workmux panes
```
