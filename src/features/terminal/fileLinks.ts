import type { IBuffer, IBufferRange, ILink, ILinkProvider, Terminal } from "@xterm/xterm";

/** A `path:line[:col]` in terminal text. `index` and `length` are string indices. */
export interface Location {
  index: number;
  length: number;
  path: string;
  line: number;
  col?: number;
}

/** The text of a logical line, and the buffer cell of each string index. */
export interface LogicalLine {
  text: string;
  /** 0-based cell `x`, buffer row `y` and cell width, per string index. */
  cells: { x: number; y: number; width: number }[];
}

/** The characters that the logical line reads on each side of the hovered row, like the xterm web link provider. */
const WINDOW = 2048;

/**
 * The logical line that holds buffer row `row` (0-based): that row joined with the soft-wrapped rows before and
 * after it, at most `WINDOW` characters on each side. The second cell of a wide character gives no text. Erased cells
 * at the end of a row give no text. Where the window cuts the line, the cut word is blanked, so no part of a longer
 * path becomes a link.
 */
export function logicalLine(buffer: IBuffer, row: number): LogicalLine {
  const rows = Math.ceil(WINDOW / Math.max(buffer.getLine(row)?.length ?? 1, 1));
  let first = row;
  while (first > 0 && row - first < rows && buffer.getLine(first)?.isWrapped) first--;
  let last = row;
  while (last - row < rows && buffer.getLine(last + 1)?.isWrapped) last++;
  const out: LogicalLine = { text: "", cells: [] };
  for (let y = first; y <= last; y++) {
    const line = buffer.getLine(y);
    if (!line) break;
    let end = line.length;
    while (end > 0 && line.getCell(end - 1)?.getChars() === "") end--;
    for (let x = 0; x < end; x++) {
      const cell = line.getCell(x)!;
      const width = cell.getWidth();
      if (width === 0) continue;
      const chars = cell.getChars() || " ";
      out.text += chars;
      for (let i = 0; i < chars.length; i++) out.cells.push({ x, y, width });
    }
  }
  const cutStart = buffer.getLine(first)?.isWrapped;
  const cutEnd = buffer.getLine(last + 1)?.isWrapped;
  if (cutStart) out.text = out.text.replace(/^\S*/u, (word) => " ".repeat(word.length));
  if (cutEnd) out.text = out.text.replace(/\S*$/u, (word) => " ".repeat(word.length));
  return out;
}

/** The 1-based, end-inclusive buffer range of the string indices `[index, index + length)`. */
export function cellRange({ cells }: LogicalLine, index: number, length: number): IBufferRange {
  const start = cells[index];
  const end = cells[index + length - 1];
  return { start: { x: start.x + 1, y: start.y + 1 }, end: { x: end.x + end.width, y: end.y + 1 } };
}

/** The longest path that a link can have: `PATH_MAX` on macOS. */
const MAX_PATH = 1024;
const QUOTES = `'"${"`"}`;
const QUOTE = /['"`]/u;
const OPENERS: Record<string, string> = { "(": ")", "[": "]", "{": "}", "<": ">" };
/** `path:line[:col]` and what follows. The path has no colon. */
const COLON = /^([^:]+):(\d+)(?::(\d+))?(.*)$/su;
/** `path(line[,col])`. */
const PAREN = /^(.+)\((\d+)(?:,\s?(\d+))?\)$/su;

/**
 * The words of `text`, split at whitespace. A word that starts with a quote, also after openers such as `("`, runs to
 * its closing quote and on. With no closing quote, it runs to the end of `text`, so no part of it becomes a link.
 */
function words(text: string): { index: number; word: string }[] {
  const out: { index: number; word: string }[] = [];
  let i = 0;
  while (i < text.length) {
    if (/\s/u.test(text[i])) {
      i++;
      continue;
    }
    const start = i;
    let quote = i;
    while (quote < text.length && text[quote] in OPENERS) quote++;
    if (QUOTES.includes(text[quote])) {
      const close = text.indexOf(text[quote], quote + 1);
      i = close < 0 ? text.length : close + 1;
    }
    while (i < text.length && !/\s/u.test(text[i])) i++;
    // `a.cs(14, 5)`: the space after the comma is in the location.
    if (/\(\d+,$/u.test(text.slice(Math.max(start, i - 12), i))) {
      const rest = /^ \d+\)\S*/u.exec(text.slice(i, i + 16));
      if (rest) i += rest[0].length;
    }
    out.push({ index: start, word: text.slice(start, i) });
  }
  return out;
}

/**
 * The location in `body`, a word with no quotes around it. `null` if it is no location. `length` is the length of the
 * location in `body`: it can end before the body, as in `app.log:12:30:45`.
 */
function parse(body: string): { path: string; line: number; col?: number; length: number } | null {
  const paren = PAREN.exec(body);
  if (paren) {
    const [, path, line, col] = paren;
    return { path, line: Number(line), ...(col !== undefined && { col: Number(col) }), length: body.length };
  }
  const colon = COLON.exec(body);
  if (!colon) return null;
  const [, path, line, col, after] = colon;
  if (after === "") {
    return { path, line: Number(line), ...(col !== undefined && { col: Number(col) }), length: body.length };
  }
  // In `app.log:12:30:45` only the line is certain. `a.rs:12.5` and `a.rs:12x` are no location.
  if (col !== undefined && /^:\d/u.test(after)) return { path, line: Number(line), length: path.length + 1 + line.length };
  return null;
}

/** Trims trailing punctuation, and closers that have no opener in the word. */
function trimEnd(word: string): string {
  for (;;) {
    const last = word[word.length - 1] as string | undefined;
    if (last === undefined) return word;
    if (".,;:!?".includes(last)) {
      word = word.slice(0, -1);
      continue;
    }
    const opener = Object.keys(OPENERS).find((o) => OPENERS[o] === last);
    if (opener && word.split(opener).length < word.split(last).length) {
      word = word.slice(0, -1);
      continue;
    }
    return word;
  }
}

/**
 * The `path:line[:col]`, `path(line,col)` and Rust `--> path:line:col` candidates in `text`. The scan is linear: it
 * splits `text` into words and parses each word whole, so no suffix of a longer path is a candidate. A quoted path
 * can have spaces: `"My File.ts:43"` or `"My File.ts":43`. A word with a `scheme://` URL is skipped: the web link
 * provider owns it. A column that more digits follow, such as in `12:30:45`, is dropped. A path of digits only, such
 * as in `12:30`, and the host `localhost` are no candidate.
 */
export function findLocations(text: string): Location[] {
  const found: Location[] = [];
  for (let { index, word } of words(text)) {
    if (word.length > MAX_PATH + 32 || word.includes("://")) continue;
    let location: ReturnType<typeof parse> = null;
    let lead = 0;
    while (word[lead] in OPENERS) lead++;
    const quote = word[lead];
    if (QUOTES.includes(quote)) {
      // A quoted path, maybe in brackets: the location is the whole quoted text, or the quoted path and the
      // `:line[:col]` after it. Closers after the quote are trimmed. A token with an unpaired quote is no location.
      const close = word.indexOf(quote, lead + 1);
      const after = trimEnd(word.slice(close + 1));
      if (close < 0 || QUOTE.test(after)) continue;
      const inner = word.slice(lead + 1, close);
      if (after === "") {
        location = parse(inner);
        if (location?.length !== inner.length) location = null;
        index += lead + 1;
      } else {
        const tail = /^:(\d+)(?::(\d+))?$/u.exec(after);
        if (tail) {
          const [, line, col] = tail;
          location = { path: inner, line: Number(line), ...(col !== undefined && { col: Number(col) }), length: close - lead + 1 + after.length };
          index += lead;
        }
      }
    } else {
      if (QUOTE.test(word)) continue;
      let body = trimEnd(word);
      // A bracket around the whole location is not in the path: `(src/a.rs:3)`, but not `(group)/page.tsx:3`.
      const closer = OPENERS[body[0]];
      if (closer && (body.endsWith(closer) || !body.includes(closer))) {
        body = body.endsWith(closer) ? body.slice(1, -1) : body.slice(1);
        index += 1;
      }
      location = parse(body);
    }
    if (!location) continue;
    const { path, line, col, length } = location;
    if (path.length > MAX_PATH || /^[\d.]*$/u.test(path) || /^localhost$/iu.test(path) || line < 1) continue;
    found.push({ index, length, path, line, ...(col !== undefined && col >= 1 && { col }) });
  }
  return found;
}

/**
 * Links each `path:line[:col]` that `check` confirms as a file. Only ⌘-click opens a link: a plain click,
 * a selection and a copy stay as they are. Each request checks again, so a new folder of the terminal counts at once,
 * also when the shell changed it with no output. When the row changes while the checks run, or before the click, as
 * after a redraw, a resize or a switch to the alternate screen, the request gives no links and the click opens nothing.
 */
export function fileLinkProvider(
  term: Terminal,
  check: (path: string) => Promise<unknown>,
  open: (location: Location) => void,
): ILinkProvider {
  return {
    provideLinks(y, callback) {
      const buffer = term.buffer.active;
      const line = logicalLine(buffer, y - 1);
      const current = () => (term.buffer.active === buffer ? logicalLine(buffer, y - 1) : undefined);
      const unchanged = (now: LogicalLine | undefined, { index, length }: Location) =>
        now?.text === line.text &&
        JSON.stringify(cellRange(now, index, length)) === JSON.stringify(cellRange(line, index, length));
      const candidates = findLocations(line.text);
      if (candidates.length === 0) return callback(undefined);
      const paths = new Map<string, Promise<boolean>>();
      for (const { path } of candidates) {
        if (!paths.has(path)) paths.set(path, check(path).then(() => true, () => false));
      }
      void Promise.all(candidates.map(({ path }) => paths.get(path)!)).then((ok) => {
        const now = current();
        const links: ILink[] = candidates
          .filter((location, i) => ok[i] && unchanged(now, location))
          .map((location) => ({
            range: cellRange(line, location.index, location.length),
            text: line.text.slice(location.index, location.index + location.length),
            activate: (event) => {
              if (event.metaKey && unchanged(current(), location)) open(location);
            },
          }));
        callback(links.length > 0 ? links : undefined);
      });
    },
  };
}
