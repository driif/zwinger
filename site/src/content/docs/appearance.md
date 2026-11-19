---
title: Appearance
description: Themes, text size, density, sidebar font size, and terminal settings.
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
New windows open directly in the last-used theme. If the saved theme cannot be read, the window opens in zwinger dark.

## Text size

The **Font size** of a custom theme sets the body text size. Text in labels, headings, controls, rail rows and the project
map scales with it. Built-in themes use `0.8125rem`.

## Density

In **Settings > Appearance > Layout**, choose **Default** or **Compact**. Compact reduces spacing and the height of rows
and controls throughout the app, including the project map. The change applies at once and persists after a restart.

## Sidebar font size

Choose **Theme default** or **12**, **13**, **14**, or **15 px** in **Settings > Appearance > Layout**. Theme default follows
the selected theme's body size. A set size changes the rail text and its controls without changing the rest of the app.
The change applies at once and persists after a restart.

## Terminal

**Settings > Terminal** sets the font, cursor, colors and keys of all terminals. Changes apply at once.

The numeric settings have these limits:

| Setting | Minimum | Maximum |
| --- | --- | --- |
| Font size | 8 | 32 |
| Line height | 1 | 2 |
| Padding | 0 | 32 |
| Scrollback lines | 1000 | 100000 |

If you type a value out of range, zwinger sets the nearest limit and shows a note, for example
`Font size: 64 is more than the maximum. Set to 32.`

**Import from Ghostty** reads the Ghostty config files and applies the settings and colors that they set. The note
shows the keys that zwinger does not use. It also shows each Ghostty value that was out of range, with its config line
and the value that the setting got, for example
`Scrollback lines: Ghostty scrollback-limit = 50000000 is more than the maximum. Set to 100000.` Ghostty uses
different units for some keys: `adjust-cell-height` is a percent that is added to a line height of 1, and
`scrollback-limit` is bytes, at 100 bytes for each line. If a key occurs more than one time, only the value that
applies gives a note.
