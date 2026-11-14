---
title: Appearance
description: Themes, the text size and what waits for the UI library.
---

Open **Settings > Appearance** (⌘,) to select the look of zwinger.

## Themes

zwinger has three built-in themes: **zwinger dark**, **zwinger light** and **High contrast**. Built-in themes are
read-only. To make your own theme, click **Duplicate** on a theme, then edit it.

- **Use** applies a theme.
- **Follow the system** uses one light theme and one dark theme, and changes between them when macOS changes its
  appearance.
- A custom theme sets the colors, the corner radius and the font size. The window shows a change at once. **Save**
  keeps it.

zwinger keeps each custom theme as one JSON file in the `themes` folder of its app data.

## Text size

All built-in themes use one body text size: `0.8125rem`. The text of zwinger's own views and the rows of the sidebar
use the body size. Secondary labels and headings are fixed steps below or above it. When a custom theme changes
**Font size**, these sizes change with it.

Controls from the UI library, for example buttons, fields, menus and selects, keep their own fixed sizes. They do not
change with **Font size** yet.

The project map keeps its own fixed size, because it places its boxes in pixels.

## Not available yet

zwinger has no density setting and no separate sidebar font size. Both need support in the UI library first.
