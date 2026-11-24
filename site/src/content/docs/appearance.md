---
title: Appearance
description: Themes, zoom, text size, density, sidebar font size, terminal settings, and the app icon.
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

### Import and export a theme

- To share a custom theme, click **Export** on its card, then choose where to save the JSON file. Built-in themes have
  no **Export**.
- To add a theme from a JSON file, click **Import theme…** in the **Theme** card, then choose the file.

zwinger checks the file before it adds the theme. It refuses a file that is larger than 64 KiB, is a link or not a
regular file, is not valid theme JSON, or uses the id of a built-in theme. A refused file adds nothing.

An import never replaces a theme. The imported theme gets a new id if a theme file with the same id exists, or if your
settings still select a theme with that id. The imported theme gets a number, for example `Sea 2`, if a custom theme or a
built-in theme has the same name. Names that differ only in spaces are the same name. A note shows the name, for example
`Imported 'Sea' as 'Sea 2'`.

An import does not select the theme. Click **Use** on its card to apply it.

New windows open directly in the last-used theme. If the saved theme cannot be read, the window opens in zwinger dark.

In **zwinger light**, the Z of the zwinger mark is a darker peach, so the mark is clear at small sizes on a light
background.

## App icon

On macOS 26 and later, the app icon follows **Icon & widget style** in **System Settings > Appearance**: **Default**
shows a light tile, **Dark** shows a dark tile, and **Clear** and **Tinted** show a clear or tinted icon. On macOS 14
and 15, the app icon is the dark tile in all modes.

## Zoom

Press ⌘+ (or ⌘=) to zoom in, ⌘− to zoom out and ⌘0 to show the actual size. The **View** menu has the same actions.
The zoom changes the size of the whole window content: text, controls, the rail and the terminals. The steps go from
50% to 200%. The terminals fit their panes again after each change. The zoom persists after a restart.

The zoom is different from the other size settings:

- **Zoom** scales all of the app by the same amount.
- **Font size** of a theme, **Density** and **Sidebar font size** change only the text or spacing that they name.
- The terminal **Font size** changes only the terminal text.

The zoom multiplies these settings. For example, a terminal font size of 13 px at 150% zoom shows like 19.5 px.

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
**Font ligatures** is off by default. Turn it on with a ligature font such as Fira Code or JetBrains Mono.
A font without ligatures shows the same text.
The Mac app joins common supported operators such as `->`, `=>` and `===`. Other ligatures in the selected font,
such as `&&` and `||`, may stay separate. No font permissions are required.

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
