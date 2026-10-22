import { openUrl } from "@tauri-apps/plugin-opener";
import { Fragment, ReactNode } from "react";
import "./Markdown.css";

// Code first, so `*` inside code is not emphasis. Links take only http(s) and mailto.
const INLINE = /(`[^`\n]+`)|\*\*([^*\n]+)\*\*|\*([^*\n]+)\*|\b_([^_\n]+)_\b|\[([^\]\n]+)\]\(((?:https?:\/\/|mailto:)[^)\s]+)\)/g;

function inline(text: string): ReactNode[] {
  const nodes: ReactNode[] = [];
  let last = 0;
  for (const m of text.matchAll(INLINE)) {
    nodes.push(text.slice(last, m.index));
    const [, code, bold, em1, em2, label, href] = m;
    const key = m.index;
    if (code) nodes.push(<code key={key}>{code.slice(1, -1)}</code>);
    else if (bold) nodes.push(<strong key={key}>{inline(bold)}</strong>);
    else if (em1 ?? em2) nodes.push(<em key={key}>{em1 ?? em2}</em>);
    else
      nodes.push(
        // The webview must not navigate away; the system browser opens the link.
        <a
          key={key}
          href={href}
          title={href}
          onClick={(event) => {
            event.preventDefault();
            openUrl(href);
          }}
        >
          {label}
        </a>,
      );
    last = m.index + m[0].length;
  }
  nodes.push(text.slice(last));
  return nodes;
}

const LIST = /^\s*([-*+]|\d+[.)])\s+(.*)$/;
const HEADING = /^(#{1,6})\s+(.*)$/;

/**
 * A small Markdown subset for model output: paragraphs, headings, lists,
 * quotes, fenced code, inline code, bold, italic and links. All text goes
 * into React text nodes, so HTML in the output is shown, not run.
 */
export function Markdown({ text }: { text: string }) {
  const lines = text.split("\n");
  const blocks: ReactNode[] = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    const key = blocks.length;
    if (line.trimStart().startsWith("```")) {
      const code: string[] = [];
      for (i++; i < lines.length && !lines[i].trimStart().startsWith("```"); i++) code.push(lines[i]);
      i++;
      blocks.push(
        <pre key={key} className="md-code">
          <code>{code.join("\n")}</code>
        </pre>,
      );
    } else if (HEADING.test(line)) {
      const [, hashes, title] = line.match(HEADING)!;
      blocks.push(
        <p key={key} className={`md-heading h${hashes.length}`}>
          {inline(title)}
        </p>,
      );
      i++;
    } else if (LIST.test(line)) {
      const ordered = /\d/.test(line.match(LIST)![1]);
      const items: ReactNode[] = [];
      for (; i < lines.length && LIST.test(lines[i]); i++) items.push(<li key={i}>{inline(lines[i].match(LIST)![2])}</li>);
      blocks.push(ordered ? <ol key={key}>{items}</ol> : <ul key={key}>{items}</ul>);
    } else if (line.startsWith(">")) {
      const quote: string[] = [];
      for (; i < lines.length && lines[i].startsWith(">"); i++) quote.push(lines[i].replace(/^>\s?/, ""));
      blocks.push(<blockquote key={key}>{inline(quote.join(" "))}</blockquote>);
    } else if (line.trim() === "") {
      i++;
    } else {
      const para: string[] = [];
      for (
        ;
        i < lines.length &&
        lines[i].trim() !== "" &&
        !lines[i].trimStart().startsWith("```") &&
        !HEADING.test(lines[i]) &&
        !LIST.test(lines[i]) &&
        !lines[i].startsWith(">");
        i++
      )
        para.push(lines[i]);
      blocks.push(
        <p key={key}>
          {para.map((l, n) => (
            <Fragment key={n}>
              {n > 0 && <br />}
              {inline(l)}
            </Fragment>
          ))}
        </p>,
      );
    }
  }
  return <div className="md">{blocks}</div>;
}
